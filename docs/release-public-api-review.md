# Public API Import Review

Rust public API surfaces reviewed for release readiness.

## Task-Focused Imports

- `styx::imports::capture` exposes capture requests, `CaptureSource`, `CaptureHandle`, backend/device types, config types, and core frame/format receive types.
- `styx::imports::pipeline` exposes `MediaPipeline`, `MediaPipelineBuilder`, `PipelineExecutionMode`, health/memory metrics, and frame transform/receive types.
- `styx::imports::codec` exposes typed codec selectors, registry/config types, codec traits, and runtime inventory helpers.
- `styx::imports::service` exposes typed service, sink, recording, and pipeline worker lifecycle events.
- Feature-gated graph/watch/preview exports remain scoped behind their existing feature gates.

## Facade Prelude

The facade prelude still intentionally re-exports the broad capture/core/codec surface for examples and applications that prefer a single import. New release APIs added during this pass are available from the prelude:

- `CaptureSource::open`, `open_with_config`, `open_with_policy`, and `pipeline`.
- `ProbedDevice::open`, `open_with_config`, `open_with_policy`, and `pipeline`.
- `PipelineExecutionMode`.
- `PipelineWorkerEvent` and `PipelineWorkerStopReason`.

## Feature Checks

Validated release-sensitive surfaces with:

- `cargo check -p styx --no-default-features`
- `cargo check -p styx --no-default-features --features async`
- `cargo check -p styx --no-default-features --features graph-pipeline`
- `cargo check -p styx-examples --no-default-features --features 'async,file-backend,netcam'`

Current release conclusion: public exports are grouped by task for narrower imports, the broad prelude remains stable for existing users, and the reviewed feature combinations compile.
