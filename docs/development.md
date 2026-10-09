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
| `nostd` | `check-nostd.sh`, which ends with `mcu-size.sh --check` |
| `perf-smoke` | `check-perf-smoke.sh` |
| `fuzz` | `cargo +nightly check` in `fuzz/` (skipped without nightly) |
| `musl-clippy` | `styx` (native) and `styx-native`, `native-spike` (all targets) for `aarch64-unknown-linux-musl` |
| `cross` | a release build of `styx` and `styx-record-cli` (native, V4L2) for `aarch64-unknown-linux-gnu`, when `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` names an aarch64 glibc linker (e.g. the HeliOS buildroot's `aarch64-linux-gcc`) |

`scripts/gate.sh --full` adds what is rarely needed: `zero_alloc` repeated
(`GATE_STRESS_RUNS`, default 10), every release feature set of `styx`
(`check-feature-combinations.sh`), the all-feature workspace clippy (needs FFmpeg), the memory
smoke, and the workspace's tests under `cargo test` (all tests of a binary in one process).
`GATE_SKIP="fuzz cross"` skips steps by name; the summary names every skipped step.

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
- **One integration-test binary per package.** Relinking test binaries is the largest write
  after an edit: each one links the whole stack below it. A package's integration tests are
  modules of `tests/it/main.rs` (one module per former `tests/*.rs` file, shared helpers in
  `tests/it/common/`); a module that needs features or a platform has the `#[cfg]` on its `mod`
  line instead of a `required-features` entry. A binary of its own is kept only for tests that
  need their own process, each listed with its reason in `check-test-targets.sh` (which fails
  when a package has a second binary that is not listed): the counting `#[global_allocator]`s
  (`styx`'s `zero_alloc`, `styx-sensor`'s and `styx-runtime`'s `no_alloc`, `styx-core-rs`'s
  `descriptor_allocations`, `styx-mcu-footprint`'s `heap`) and `styx`'s `metrics`, which checks
  the process-wide count of camera service clients (under `cargo test` the other modules'
  services would run beside it in the same process). Run one former file's tests with a filter
  on its module: `cargo test -p styx-softisp --test it golden`. See the measurements below.
- **Three release builds for the perf smoke instead of five**, grouped so that each binary keeps
  what it measures (see `check-perf-smoke.sh`); `check-mem-smoke.sh` shares the libjpeg-turbo +
  MCAP one in CI.
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

The linker is not a lever: Rust 1.99 links `x86_64-unknown-linux-gnu` with the bundled `rust-lld`
already, so the repository has no `.cargo/config.toml` (one would also collide with the untracked
one used to patch Lemnos or Daedalus to a local checkout).

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
