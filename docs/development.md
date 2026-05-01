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
cargo fmt -p styx-core-rs -p styx-capture -p styx-codec -p styx-libcamera -p styx -p styx-v4l2 -p styx-examples -- --check
./scripts/check-file-sizes.sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
bash ./scripts/check-feature-combinations.sh
cargo tree -d --workspace --no-default-features
cargo doc --workspace --no-deps
```

## Examples

User-facing examples live under `examples/`. Prefer exercising new end-to-end behavior there before adding heavier CI-specific fixtures.

See [`testing.md`](testing.md) for the default and example-focused validation surfaces.

## Tooling

- Rust 1.94.0 is the release toolchain and MSRV for this workspace.
- Rust toolchain is pinned in [`rust-toolchain.toml`](../rust-toolchain.toml)
- Root dependency versions are aligned in [`Cargo.toml`](../Cargo.toml)
- Local validation entrypoint lives in [`scripts/ci.sh`](../scripts/ci.sh)
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
- libcamera camera lookup timeout/poll, request requeue stall timeout, completed-request poll timeout, idle-drain timeout/poll, control-response timeout, probe cache TTL, idle-stop policy, request-pool prefault policy, and processed-stream role
- netcam request/connect/read timeouts, reconnect backoff, frame send timeout, and stop-poll interval

Environment variables are deployment/debug overrides. Release-facing code should prefer typed
`StyxConfig`, codec policy/registry APIs, or libcamera manager config APIs.

- `STYX_LIBCAMERA_PROBE_CACHE_MS`: overrides the shared manager probe-cache TTL.
- `STYX_LIBCAMERA_STOP_WHEN_IDLE`: overrides libcamera idle-stop behavior.
- `STYX_LIBCAMERA_PREFAULT_REQUEST_POOLS`: overrides libcamera request-pool prefaulting.
- `STYX_LIBCAMERA_PROCESSED_STREAM_ROLE`: overrides the processed stream role.
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
- Graph node IDs remain strings at Styx API boundaries because Daedalus owns `NodeHandle`/`NodeId`
  semantics; keep string usage there localized to graph registration/descriptors.

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

- `daedalus` is pinned to revision `ae98541285caeb48da7ff2fa3c101d5daf15e482`; prefer a crates.io release or tag when one is available.
- Duplicate `bindgen` versions come from `v4l`, `libcamera-sys`, and `ffmpeg-sys-next`; this is transitive backend/tooling churn rather than direct workspace drift.
- The duplicate `bitflags` stack from `v4l`/`v4l2-sys-mit` is accepted for this release because it is isolated behind the `v4l2` feature and upstream `v4l` currently pins the older build-time path.
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
