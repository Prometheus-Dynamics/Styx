# Release Readiness Tasks

## Build And CI

- [x] Fix `styx --all-features` build failure in graph sink registration.
  - `crates/styx/src/graph/sinks.rs` passes an owned label by reference into `framelease_node_decl`, which currently requires `&'static str`.
  - Decide whether `framelease_node_decl` should own the label, accept `impl Into<String>`, or restrict caller labels to static strings.

- [x] Add CI coverage for release feature combinations.
  - Include `cargo check --workspace --all-targets`.
  - Include `cargo clippy --workspace --all-targets -- -D warnings`.
  - Include `cargo check -p styx --all-features`.
  - Add targeted checks for heavy feature groups such as `async`, `netcam`, `file-backend`, `graph-pipeline`, `codec-ffmpeg`, `v4l2`, and `libcamera` where CI hardware allows.

## Async And Runtime Behavior

- [x] Rework or clarify `MediaPipeline::next_async_result`.
  - It awaits frame receipt but runs decode, encode, graph, hooks, and sinks synchronously on the current Tokio worker.
  - Prefer making the blocking-pool path the obvious default for async users.

- [x] Reconsider `MediaPipeline::spawn_async_worker` ergonomics.
  - The current name can imply fully async processing, but frame processing is synchronous.
  - Consider renaming, deprecating before release if possible, or documenting it as receive-only async.

- [x] Strengthen async teardown guidance.
  - `CaptureHandle::Drop` can synchronously join worker threads.
  - Document that async callers should use `stop_async`.
  - Consider adding an explicit nonblocking abort/shutdown path for async-owned handles.

## Locking And Failure Semantics

- [x] Audit production `Mutex` and `RwLock` usage that calls `unwrap()` or `expect()`.
  - Focus on runtime code, not tests or examples.
  - Candidate areas: codec registry locks, V4L2 mmap state, V4L2 external backing drop, shared codec state, and service/runtime shared state.

- [x] Replace lock poisoning panics in public/runtime paths.
  - Prefer `parking_lot` where poisoning is not useful.
  - Otherwise recover from poisoned locks or return typed errors.

- [x] Review drop-time behavior for frame backings.
  - Ensure drop paths do not panic and do not block unexpectedly.
  - Keep V4L2/libcamera backing recycle paths best-effort and observable.

## API Ergonomics

- [x] Split or document smaller import surfaces.
  - The facade prelude is convenient but broad.
  - Add task-focused examples or modules for capture-only, pipeline, codec, graph, watch, and service workflows.
  - Added `styx::imports::{capture,pipeline,codec,service,watch}` plus feature-gated graph/recording import modules.
  - Documented in `docs/api-ergonomics.md`.

- [x] Review whether common simple tasks require too much setup.
  - Capture one frame.
  - Start a raw frame pipeline.
  - Decode MJPEG to RG24.
  - Record frames.
  - Start async netcam capture.
  - Documented focused recipes in `docs/api-ergonomics.md`.

- [x] Add more standard trait impls where they reduce caller code.
  - Review public config, selector, descriptor, and ID types for useful `From`, `TryFrom`, `AsRef`, `Display`, `FromStr`, and builder defaults.
  - Keep conversions typed and avoid expanding stringly typed APIs.
  - Added `AsRef<ProbedDevice>`, `From<ProbedDevice> for CaptureSource`, and `From<CaptureSource> for ProbedDevice`.
  - Reviewed existing typed selector/ID impls and documented the policy for future additions.

## Performance And Memory

- [x] Keep CPU-heavy media work off Tokio core workers.
  - Make `spawn_blocking_worker` or `spawn_tokio_worker` the recommended async pipeline path.
  - Add examples that show the preferred runtime pattern.

- [x] Review queue locking overhead in hot paths.
  - `BoundedTx::send` takes a wait-state lock even for nonblocking sends.
  - Confirm this is intentional for wake correctness, or split fast-path send from blocking coordination if benchmarks justify it.
  - Kept the lock for wake correctness and documented the invariant in `crates/core/src/queue.rs`.

- [x] Expand runtime observability around blocking and drops.
  - Surface queue send timeouts, async waits/wakes, dropped frames, external backing counts, and worker join/teardown latency in examples and docs.

- [x] Benchmark representative release paths.
  - Raw virtual capture.
  - V4L2 mmap capture.
  - libcamera capture.
  - MJPEG decode.
  - Raw decoder transforms.
  - Netcam MJPEG async path.
  - Pipeline worker under backpressure.
  - Added a release benchmark matrix in `docs/performance.md`.

## Dependencies And Features

- [x] Keep dependencies centralized at workspace level.
  - Current manifests mostly do this well; preserve it as new deps are added.

- [x] Review heavy optional dependencies before release.
  - `bevy`, `ffmpeg-next`, `reqwest`, `image`, `libcamera`, and codec backends should stay feature-gated.
  - Confirm default features stay minimal.

- [x] Track transitive duplicate dependencies.
  - Current duplicates appear mainly through `v4l`/`bindgen` and dev tooling.
  - Recheck with `cargo tree -d` before release.

## Code Organization

- [x] Keep Rust source files below the 800-line target.
  - Current largest file observed was under the limit.
  - Watch files near the limit: capture request, session builder/runtime, codec ffmpeg encoder/decoder, V4L2 backend, libcamera backend, metrics, and codec registry.

- [x] Split files only when responsibilities become unclear.
  - Prefer preserving current module boundaries unless a file crosses the line-count target or mixes unrelated ownership.

- [x] Remove unnecessary lint suppressions.
  - Revisit `allow(dead_code)`, `allow(unused_imports)`, and targeted clippy allows.
  - Keep only release-policy or feature-matrix justified suppressions.
  - Reviewed remaining suppressions and documented release policy in `docs/development.md`.

## Observability

- [x] Add a release debugging guide.
  - Show how to enable `tracing`.
  - Show where to read health reports, queue stats, memory stats, and last worker/control errors.

- [x] Ensure errors are visible after async worker failures.
  - Continue surfacing last worker errors through `last_error` and health reports.
  - Add tests or examples for failure diagnostics where practical.

## Verification

- [x] Run and record final release checks.
  - `cargo fmt --check`
  - `cargo check --workspace --all-targets`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo check -p styx --all-features`
  - `cargo test --workspace`
  - Feature-specific tests and examples required for the release target platforms.
  - Passed locally with `CARGO_TARGET_DIR=/tmp/styx-release-readiness-target`:
    - `cargo fmt --check`
    - `cargo check --workspace --all-targets`
    - `cargo clippy --workspace --all-targets -- -D warnings`
    - `cargo check -p styx --all-features`
    - `cargo test --workspace`
    - `cargo check -p styx --no-default-features --features async,netcam,file-backend,graph-pipeline,codec-ffmpeg,v4l2,libcamera`
