# Development

Styx follows the shared Prometheus Dynamics workspace layout:

- `crates/`: published and internal Rust crates
- `docs/`: repository-level guidance
- `testing/`: validation notes and CI-facing test surfaces
- `.github/workflows/`: GitHub Actions pipelines

## Validation Surface

Use these commands for the default local validation loop:

```bash
./scripts/repo-clean.sh
cargo fmt --all --check
./scripts/check-file-sizes.sh
./scripts/check-test-targets.sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
bash ./scripts/check-feature-combinations.sh
cargo tree -d --workspace --no-default-features
cargo doc --workspace --no-deps
```

## The Gate

`scripts/gate.sh` runs the gate before a merge to `dev` with as little compile work as covers
it; `scripts/gate.sh --list` prints its steps:

| Step | What it runs |
| --- | --- |
| `fmt`, `file-sizes`, `test-targets` | `cargo fmt --all --check`, `check-file-sizes.sh`, `check-test-targets.sh` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` (default features) |
| `clippy-features` | `styx` with `native,v4l2,async,libcamera,daedalus,frame-socket,preview,codec-turbojpeg` and `styx-record-cli` with all its features, in one pass |
| `test` | the workspace's tests with default features: `cargo nextest run --workspace`, then `cargo test --workspace --doc` |
| `test-features` | `styx` and `styx-record-cli` in one build: `styx` with `native,daedalus,codec-turbojpeg,replay-mcap,preview`, `styx-record-cli` with all features (styx's native tests, `zero_alloc` with Daedalus, the recorder, and `camera_service`, `scaled_planning`, `shared_planning` and `preview`) |
| `nostd` | `check-nostd.sh`: the `no_std` crates linted in seven groups, each for all five targets in one cargo run, their host tests, the MCU images and `mcu-size.sh --check` |
| `perf-smoke` | `check-perf-smoke.sh` |
| `fuzz` | `cargo +nightly check` in `fuzz/` (skipped without nightly) |
| `musl-clippy` | `styx` (native) and `styx-native`, `native-spike` (all targets) for `aarch64-unknown-linux-musl` |
| `cross` | a release build of `styx` and `styx-record-cli` (native, V4L2) for `aarch64-unknown-linux-gnu`, when `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` names an aarch64 glibc linker (e.g. the HeliOS buildroot's `aarch64-linux-gcc`) |

`scripts/gate.sh --full` adds what is rarely needed: `zero_alloc` repeated
(`GATE_STRESS_RUNS`, default 10), every release feature set of `styx`
(`check-feature-combinations.sh`), the all-feature workspace clippy (needs FFmpeg), the memory
smoke, and the workspace's tests under `cargo test` (all tests of a binary in one process).
`GATE_SKIP="fuzz cross"` skips steps by name; the summary names every skipped step.

`scripts/gate.sh --changed` is the dev loop: it runs only what a change since the merge base with
`dev` can affect (`--changed=BASE` for another base). `scripts/affected.sh` lists the packages:
those with changed files and every workspace package that depends on them, transitively. A step
runs when one of its packages is affected (`clippy-features`, `test-features` and `cross`:
`styx` or `styx-record-cli`; `nostd`: the `no_std` crates and examples; `perf-smoke`:
`styx-examples`; `fuzz`: the fuzz project's dependencies; `musl-clippy`: `styx`, `styx-native`,
`native-spike`), nextest runs only the affected packages' tests, and `fmt` checks only the
changed `.rs` files. Builds stay `--workspace` with the gate's feature sets, so `--changed`
shares every artifact with the full gate (a `-p` subset would resolve features differently and
compile the dependencies again). A change outside the packages that builds read (`Cargo.toml`,
`Cargo.lock`, `scripts/`, `testing/`, a package's data files) runs everything; `docs/` and
Markdown run only the cheap checks. Before a merge to `dev`, run the full gate (or let CI run
it): `--changed` trusts the dependency graph (doctests included: they run for the affected packages).

How the gate keeps the work small:

- **Three feature sets instead of five.** `styx` and the crates under it are compiled for the
  workspace's defaults (linted and tested), the `clippy-features` set and the `test-features`
  set: four compilations of the stack for the host, where the former gate made seven (two more
  clippy passes, and test builds for the native tests, the recorder and `zero_alloc` apart).
- **nextest** runs every test binary at once, each test in its own process (process-global state
  such as `zero_alloc`'s counting allocator or a static lock never sees another test). Doctests,
  which nextest does not run, follow with `cargo test --doc`. Without `cargo-nextest` installed
  the gate falls back to `cargo test`.
- **Debug info.** `[profile.dev]` emits line tables only for the workspace's crates (backtraces keep
  file, line and function) and none for dependencies. A clean `cargo test --workspace --no-run`
  writes 3.8 GB instead of 7.2 GB, and an edit to `styx-core` relinks 1.8 GB of test binaries
  instead of 3.9 GB. `CARGO_PROFILE_DEV_DEBUG=true` gives the workspace full debug info (a
  rebuild).
- **The numeric crates at `opt-level = 1`** (`styx-algo`, `styx-tune`, `styx-softisp`,
  `styx-gpuisp`, debug builds only; overflow checks and debug assertions stay on): their
  simulations and fits dominate the test run, which drops from about 190 to 70 CPU-seconds
  (the slowest test from 20 s to 4 s) for about 70 CPU-seconds more when they are compiled.
- **No empty test binaries.** Binaries without tests are `test = false` (`check-test-targets.sh`
  fails if one gains a test), and integration tests that need features name them in
  `required-features`, so `cargo test` no longer builds and links a harness with no tests in it.
- **Three release builds for the perf smoke instead of five**, grouped so that each binary keeps
  what it measures (see `check-perf-smoke.sh`); `check-mem-smoke.sh` shares the libjpeg-turbo +
  MCAP one in CI.
- **Few cargo runs for the `no_std` checks.** `check-nostd.sh` lints the `no_std` crates in seven
  groups, each group in one `cargo clippy` for all four bare-metal and wasm targets and the host
  (`--target` repeated), where it made 85 runs, one per crate, feature set and target. Each run
  costs seconds of start-up on a busy disk, and one run builds all targets in parallel. Grouping
  is only valid while no package gets other features in the group than alone (features are
  unified across a run's packages and targets): `check-nostd.sh --verify`, run by CI, proves it
  with `cargo tree` for every crate and target. The MCU images stay one target per run (their
  dependencies' features differ between Cortex-M7 and Cortex-M0+).
- **`zero_alloc` once.** Its counts are exact and the flake behind the repeats is fixed; a repeat
  proves nothing new. `--full` keeps a stress run. `mcu-size.sh --check` runs once, inside
  `check-nostd.sh`.
- **One set of RUSTFLAGS.** The gate sets none, so no step invalidates another's artifacts. Do not
  export `RUSTFLAGS` or `CARGO_INCREMENTAL` around it: either rebuilds every workspace crate.

Measured on a 24-thread machine shared with other builds (CPU seconds are user + sys; the former
gate with `zero_alloc` three times; Daedalus `ed7ddde` for both):

| | Former gate | `scripts/gate.sh` |
| --- | --- | --- |
| From a clean `target/` | 2080-2340 CPU-s, 21.2 GB written | 1880 CPU-s, 12.9 GB written |
| After an edit to `styx` | 536 CPU-s, 5.9 GB written | 335 CPU-s, 2.8 GB written |
| `target/` afterwards | 19.3 GB | 11.3 GB |

On a disk that is busy (as here, a shared spinning disk), the bytes written decide the wall time:
the former gate took 600 s to 73 min from clean depending on the other load.

The linker is a small lever and left to the developer (see [Faster local
builds](#faster-local-builds)): Rust 1.99 links `x86_64-unknown-linux-gnu` with the bundled
`rust-lld` already, and the repository has no `.cargo/config.toml` (one would also collide with
the untracked one used to patch Lemnos or Daedalus to a local checkout).

What the gate no longer covers that the former manual gate did:

- `styx`'s feature-set clippy runs with `replay-styxrec` also enabled (pulled in by
  `styx-record-cli`'s dev-dependency in the shared pass), and `styx-record-cli`'s with that whole
  `styx` feature set rather than only what the recorder enables. A warning that only appears with
  exactly the old feature set (say, an import unused without `replay-styxrec`) is not caught.
- The native tests, the recorder's tests and `zero_alloc` with Daedalus run in one build with each
  other's features (and libjpeg-turbo, MCAP replay and previews) enabled, not each with only its
  own. Every test of the former runs is still run (checked by name); tests that need libjpeg-turbo
  absent run in the default-feature `test` step.
- Tests run in one process per test (nextest), not in one process per test binary, except under
  `--full`.
- `zero_alloc` runs once, not 3 to 13 times (`--full` repeats it).
- `styx-algo`, `styx-tune`, `styx-softisp` and `styx-gpuisp` are tested at `opt-level = 1`, not 0
  (the same semantics: overflow checks and debug assertions are on in both).
- Example binaries with no tests are no longer built as test harnesses (they still build as
  binaries for the examples' smoke test and are linted by clippy).

CI caches `target/` with `CARGO_INCREMENTAL=0` (set by `Swatinem/rust-cache`); from clean that
writes about a quarter less than an incremental build. Locally, keep incremental builds:
switching `CARGO_INCREMENTAL` rebuilds every workspace crate, and incremental rebuilds after an
edit cost a third to a half of the CPU.

## Faster local builds

On a busy machine a build waits on the disk more than on the CPU: wall time follows the bytes
written to the build directories. **Recommended: put them on the fastest disk you have** (an SSD,
or a tmpfs when the RAM allows). It was the biggest lever measured here: the same test build took
24 s on tmpfs and 35 s to 55 min on the busy spinning disk. It is per developer and changes
nothing for anyone else:

```bash
export STYX_BUILD_ROOT=/fast/disk/styx-build   # an SSD or a tmpfs, not the disk of the sources
. scripts/dev-env.sh                           # in each shell, or from ~/.bashrc
```

`scripts/dev-env.sh` sets `STYX_BUILD_ROOT` and two smaller levers, each opt-in:

- **`STYX_BUILD_ROOT`**: cargo's intermediate files (`CARGO_BUILD_BUILD_DIR`) go to
  `$STYX_BUILD_ROOT/<hash of the checkout's path>`, one directory per checkout, on a faster disk
  than `target/`; the final binaries stay in `target/` (scripts find them as before). `cargo
  clean` removes both; `rm -rf "$STYX_BUILD_ROOT"/*` is always safe. Each checkout's
  intermediate files take what `target/` took (about 5 GB after `clippy` and the tests, 11 GB
  after the whole gate), so size the disk for the worktrees kept around.
- **sccache** (`RUSTC_WRAPPER=sccache`, on when sccache is installed; `STYX_SCCACHE=0` turns it
  off). A new worktree, another checkout or a `cargo clean` then compiles only Styx's own crates
  and takes the dependencies from the cache. sccache cannot cache incremental compilations (the
  workspace's crates in debug builds), proc-macros, binaries or build scripts, so debug builds
  gain little; release builds (the perf smoke, builds for the device) gain the most. Switching it
  on or off rebuilds nothing. Do not export `CARGO_TARGET_DIR` with it: sccache hashes every
  `CARGO_*` variable, so a different target directory misses the whole cache (use
  `--target-dir` instead, as `scripts/cross-aarch64.sh` does).
- **mold** for host links, in an untracked `.cargo/config.toml` (in the checkout, or in a parent
  directory for every worktree under it, or `~/.cargo/config.toml`):

  ```toml
  [target.x86_64-unknown-linux-gnu]
  rustflags = ["-C", "link-arg=-fuse-ld=mold"]
  ```

  It halves the CPU of relinking the test binaries after an edit; the wall time stays (links run
  in parallel and write the same bytes). Set it for good or not at all: the linker is part of
  every crate's fingerprint, so each switch rebuilds everything (which is why `dev-env.sh` does
  not set it).

Rejected: a `CARGO_TARGET_DIR` shared by all worktrees. It would build each dependency once,
but every build in every worktree would wait on one lock, and worktrees would overwrite each
other's final binaries (`target/release/perf_smoke` of one worktree measured for another).
sccache shares the dependencies without either problem.

Measured on the 24-thread development machine shared with other builds (load 20 to 180; CPU
seconds are user + sys of cargo, rustc, linkers and the sccache server; "tmpfs" stands for a fast
disk):

| | CPU-s | Wall | Written |
| --- | --- | --- | --- |
| `cargo test --workspace --no-run`, new worktree, target/ on the busy disk | 344 | 35 s (load 64) to 55 min (load 100) | 4.0 GB |
| the same, on tmpfs | 335 | 24 s (load 70) | 0 on disk |
| the same, sccache warm from another worktree, tmpfs | 320 (81% of cacheable crates hit) | 28 s | 0 on disk |
| `cargo clippy --workspace --all-targets` after it, without / with sccache | 106 / 100 | 15 s / 43 s | 0.75 GB on disk |
| release build of `perf_smoke` and `file_replay_perf`, without / with sccache (other worktree) | 114 / 52 | 15 s / 15 s | |
| relink after an edit to `styx` (`cargo test --no-run`), rust-lld / mold | 21, 19 / 11, 9 | 5-6 s / 4-6 s | |
| relink after an edit to `styx-core`, rust-lld / mold | 42 / 24 | 5 s / 8 s | |

### Target directories

Every checkout and worktree has its own `target/` (5 GB after `clippy` and the tests, 11 GB
after the gate, far more over weeks of feature sets and profiles), and deleting one is costly:
`cargo clean` or `rm -rf` unlinks hundreds of thousands of small files (incremental caches
above all), and on a busy spinning disk that takes hours and slows every other build. So:

- **Fewer of them.** sccache shares the compiled dependencies between worktrees, so a new
  worktree costs its own crates only. With `STYX_BUILD_ROOT` every checkout's intermediate files
  sit under one directory on a disk of your choice, which is one place to watch and to clean.
- **Delete without waiting.** `scripts/sweep-targets.sh` lists every worktree's `target/` (and
  `target-*/`, and the directories under `STYX_BUILD_ROOT`) with the days since its last build
  (`--sizes` adds sizes, slowly); `--delete DAYS` deletes those older than that, `--delete-path P`
  one directory. Each is renamed aside at once and removed in the background at idle disk
  priority (`ionice -c3`), so nothing waits for it. The same by hand:
  `mv target .target-trash && ionice -c3 nice rm -rf .target-trash &`.
- **Remove a worktree with its target.** `git worktree remove` deletes the ignored `target/`
  too, file by file while you wait; move it aside first:
  `scripts/sweep-targets.sh --delete-path ../Styx-x/target && git worktree remove ../Styx-x`.
- **btrfs: a subvolume per target directory** deletes in a moment (`btrfs subvolume delete`,
  which `sweep-targets.sh` uses when it can) instead of file by file. Create it before the first
  build (`btrfs subvolume create target`). Deleting a subvolume as a user needs the filesystem
  mounted with `user_subvol_rm_allowed` (or root); without it a user can only remove it once
  emptied, which is the slow part again. A choice for the machine's owner.

## Building for the device

`scripts/cross-aarch64.sh` builds for the Raspberry Pi CM5 (64-bit Arm Linux) in one of two ways:

```bash
# glibc, against a Buildroot output (only read): what links libcamera or other C libraries
export STYX_AARCH64_BUILDROOT=/path/to/buildroot-output
scripts/cross-aarch64.sh -p styx-record-cli --features styx-record-cli/libcamera --out out/cm5
scripts/cross-aarch64.sh -p styx-examples --features frame-socket,libcamera \
    --bins hop_breakdown,libcamera_format_check --out out/cm5
# musl, static, linked by rust-lld: no toolchain or sysroot, runs on any aarch64 Linux
scripts/cross-aarch64.sh --musl -p styx-record-cli \
    --features styx-record-cli/native,styx-record-cli/v4l2 --out out/cm5
```

The Buildroot output (the directory holding `host/` and `staging/`) can also come from
`STYX_AARCH64_BUILDROOT`; the script takes the compilers, the sysroot and the C++ include path
for bindgen (libcamera-sys) from it. Each sysroot gets its own target directory
(`target/aarch64-<name>-<hash>`, `target/aarch64-musl`): the C parts depend on the sysroot,
which cargo does not track. `--bins a,b` / `--examples a,b` pick binaries, `--out DIR` copies
them stripped with a `SHA256SUMS`. The musl build compiles C code that needs no libc (criterion's
`alloca`) with the host's clang; anything that needs a libc needs the glibc build.

The default profile is `device`: release code generation (`opt-level = 3`), but incremental and
in 256 codegen units, so a rebuild after an edit costs a fraction of a release build.
`--profile release` builds what ships.

Measured on this development host (24 threads, load 25 to 70 from other builds; wall times are
noisy, CPU is user + sys). The perf smoke's five binaries (`luma_perf`, `shared_perf`,
`perf_smoke`, `file_replay_perf`, `encode_perf`) built with each profile:

| | `release` | `device` |
| --- | --- | --- |
| cold build, dependencies included | 67 s wall, 338 s CPU | 57 s wall, 384 s CPU |
| rebuild after a new function in `styx-core` (two edits) | 61 s and 79 s wall, 151 and 153 s CPU | 20 s and 16 s wall, 10 s CPU |
| rebuild after a comment-only edit in `styx-core` (two edits) | 27 s and 41 s wall, 123 and 131 s CPU | 4.5 s and 11 s wall, 5 to 8 s CPU |

The device profile's runtime is a host proxy only: the perf smoke's metrics on the host, the
p95 ratio device / release (medians of five interleaved runs per profile): decode 640x360 1.05,
luma pyramid decode 1.07, shared 3x luma 0.93, served luma 1.06, `rotate90_720p` 1.60 (p50 0.80
and 1.01 ms), `mirror_720p` 1.18, file replay 0.95, MJPEG encode 1.01. The spread between runs
of one profile is as large as these differences (`rotate90_720p` p95 from 1.3 to 4.8 ms), so no
runtime cost is resolved on this host. The CM5 (Cortex-A76) is not measured: the device profile's
runtime there is unknown until it runs on the device. Ship and benchmark `release`.

### Tests on the device

```bash
scripts/cross-aarch64.sh --musl --tests -p styx-core-rs -p styx-sensor   # or --sysroot DIR
scripts/device-tests.sh target/device-tests/musl                        # on DEVICE (root@helios)
scripts/device-tests.sh target/device-tests/musl --qemu                 # here, under qemu-aarch64
scripts/device-tests.sh target/device-tests/musl --only styx_core -- simd
```

`--tests` builds the packages' test binaries (`cargo test --no-run`, the dev profile unless
`--profile`) into a bundle, from a mirror of the working tree at `STYX_DEVTEST_SRC`
(`/tmp/styx-devtest-src`): tests find their fixtures through paths fixed at compile time
(`env!("CARGO_MANIFEST_DIR")`), and the run script copies the same mirror to the same path on the
device. `device-tests.sh` copies the bundle into `/tmp` on the device, then runs every binary from
its package's directory inside `scripts/with-device-lock.sh` (`WAIT=1` waits for the lock),
stopping PhotonVision first when it runs and starting it again from a trap on any exit. Arguments
after `--` go to every test binary (a filter, `--ignored` for the hardware tests). The exit code
is 0 when every binary passed. `--qemu` runs a musl bundle here instead (the NEON kernels
included): `styx-core-rs` and `styx-sensor`'s 242 tests take 13 s.

## Examples

User-facing examples live under `examples/`. Prefer exercising new end-to-end behavior there before adding heavier CI-specific fixtures.

See [`testing.md`](testing.md) for the default and example-focused validation surfaces.

## Tooling

- Rust 1.99.0 is the release toolchain and MSRV for this workspace.
- Rust toolchain is pinned in [`rust-toolchain.toml`](../rust-toolchain.toml)
- Root dependency versions are aligned in [`Cargo.toml`](../Cargo.toml)
- Local validation entrypoints live in [`scripts/gate.sh`](../scripts/gate.sh) (the gate before merging to `dev`) and [`scripts/ci.sh`](../scripts/ci.sh) (what CI runs)
- Local cleanup entrypoint lives in [`scripts/repo-clean.sh`](../scripts/repo-clean.sh)
- CI entrypoints live in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)

## Dependency Policy

- Library-facing error types use `thiserror`.
- Shared runtime instrumentation should use `tracing` instead of introducing parallel logging stacks.
- Backend or codec alternatives stay behind stable feature names and should only expand when they represent a real media/runtime tradeoff.

## Runtime Tunables

Runtime timing and capacity defaults should be named near `StyxConfig`/`CaptureTunables`/`NetcamTunables`, not embedded as anonymous sleeps in backend workers. The current configurable surfaces include:

- capture queue depth and pool sizing
- V4L2 mmap poll timeout, send timeout, and worker error backoff
- libcamera camera lookup timeout/poll, request requeue stall timeout, completed-request poll timeout, idle-drain timeout/poll, control-response timeout, probe cache TTL, idle-stop policy, request-pool prefault policy, processed-stream role, buffer memory, and pyramid level
- netcam request/connect/read timeouts, reconnect backoff, frame send timeout, and stop-poll interval

Environment variables are deployment/debug overrides. Release-facing code should prefer typed
`StyxConfig`, codec policy/registry APIs, or libcamera manager config APIs.

- `STYX_LIBCAMERA_PROBE_CACHE_MS`: overrides the shared manager probe-cache TTL.
- `STYX_LIBCAMERA_STOP_WHEN_IDLE`: overrides libcamera idle-stop behavior.
- `STYX_LIBCAMERA_PREFAULT_REQUEST_POOLS`: overrides libcamera request-pool prefaulting.
- `STYX_LIBCAMERA_PROCESSED_STREAM_ROLE`: overrides the processed stream role.
- `STYX_LIBCAMERA_BUFFER_MEMORY`: `auto`, `libcamera` or `dma-heap`; overrides where libcamera
  capture buffers are allocated.
- `STYX_LIBCAMERA_DEBUG`: enables extra libcamera debug probing.
- `STYX_FFMPEG_DECODER_THREADS`: overrides the default FFmpeg decoder thread count.

Test-only variables such as `STYX_TEST_IMAGE` and `STYX_SKIP_DOCKER_BUILD` are limited to the Docker
facade test harness and should not become runtime configuration.

Frame pacing derived from advertised media FPS may remain local to file, simulation, and MJPEG pacing code because the device/media format is the source of truth.

Prefer `StyxConfig` builder methods in examples and application code. Direct `CaptureTunables` or
`NetcamTunables` literals should include `..Default::default()` so release-added fields inherit safe
defaults.

Capture receive naming is intentionally explicit for this release:

- `recv` polls without waiting and returns `RecvOutcome::Empty` when no frame is ready.
- `recv_blocking(wait)` waits up to a bounded duration and maps timeout to `RecvOutcome::Empty`.
- `recv_timeout(wait)` exposes the lower-level `RecvWaitOutcome::Timeout` variant.
- `recv_forever` waits indefinitely until data or closure.
- `recv_async` waits asynchronously when the `async` feature is enabled.

Do not add compatibility aliases for receive methods before 1.0; examples and docs should use the explicit names above.

Public selector/ID policy for this release:

- Backend selection uses `BackendKind`.
- Runtime codec selection uses `CodecSelector`; codec implementation keys use `CodecImplementationId`.
- Service sink categories use `SinkKind`.

## Logging Policy

- Expected retry/backoff and queue-pressure events stay at `debug`.
- Per-frame hot-path diagnostics stay at `trace` or `debug` and must include structured fields such as `backend`, `stage`, or `drop_reason`.
- Actionable setup, connection, backend, and graph failures use `warn` or `error`.
- Avoid new `warn`/`error` logs inside successful per-frame loops unless the condition is abnormal and operator-actionable.

## Lint Suppression Policy

Keep `allow` and `expect` usage narrow and auditable before release:

- Prefer deleting dead code or moving it behind a feature gate over adding `allow(dead_code)`.
- Allow unused prelude/facade imports only where the public export surface intentionally changes by feature combination.
- Keep clippy suppressions local to the smallest item and include the design reason when it is not obvious from the code.
- Do not add broad crate-level suppressions for release code.
- `expect` is acceptable for static invariants, tests, examples, and benchmark setup. Public/runtime paths should return typed errors or use non-poisoning locks when poisoning is not meaningful.

The current reviewed suppressions are feature-matrix or implementation-shape allowances. Revisit them when the related feature surface changes rather than carrying them as compatibility promises.

## Release Risk Notes

Before tagging a release, re-run:

```bash
cargo tree -d -p styx --all-features
```

Accepted dependency risks for the current release surface:

- `daedalus` (only for `styx-core-rs/daedalus`) is pinned to a commit of Daedalus `dev` by git; move to a crates.io release when 2.0 is published ([daedalus.md](daedalus.md)).
- Duplicate `bindgen` versions come from `libcamera-sys` and `ffmpeg-sys-next`; this is transitive backend/tooling churn rather than direct workspace drift.
- Duplicate `thiserror`, `rustix`, `nix`, and `toml_edit` versions are currently pulled by optional backend, preview, graph, and build-time dependency trees. Keep them visible in release notes unless upstream dependency upgrades collapse them.
- Heavy dependencies remain feature-scoped: Bevy under `simulation-bevy`, FFmpeg under FFmpeg codec/video features, Minifb under `preview-window`, and Reqwest under netcam/async paths.
- Keep release examples and docs on `--no-default-features` plus the narrow feature set they need. Avoid grouping `simulation-bevy`, `preview-window`, `netcam-video`, `codec-ffmpeg`, and `libcamera` together unless the example explicitly demonstrates that combined stack.
- Rayon is an intentional `styx-codec` core dependency for raw pixel conversion throughput and thread-local decode pool cleanup; do not feature-gate it unless a measured single-threaded codec profile becomes a release target.

Accepted runtime behavior for the current async surface:

- The `async` feature means async receive/control helpers and async netcam workers; it does not make
  every capture backend internally nonblocking.
- `CaptureRequest::start_with_policy_async` only makes retry backoff sleeps yield the Tokio runtime; backend startup remains synchronous because the camera APIs are sync-first.
- For async services where startup/probing latency matters, construct and start capture from an application-owned blocking task or worker thread, then move the returned `CaptureHandle` or `MediaPipeline` back into async code.
- `MediaPipeline::next_async` awaits capture receive asynchronously, but decode, encode, graph execution, hooks, and sinks still run synchronously on the calling task.
- Prefer `MediaPipeline::spawn_blocking_worker`, `spawn_tokio_worker`, the blocking `spawn_worker`, or an application-owned worker thread for CPU-heavy decode/encode pipelines so processing does not run on Tokio core worker tasks.
- Use `CaptureHandle::stop_async` or `stop_async_in_place` before dropping handles in Tokio tasks when teardown latency matters.
