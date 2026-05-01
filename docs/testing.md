# Testing

Styx splits validation into default workspace checks and facade-example coverage.

## Default Surface

- `cargo fmt -p styx-core-rs -p styx-capture -p styx-codec -p styx-libcamera -p styx -p styx-v4l2 -p styx-examples -- --check`
- `./scripts/check-file-sizes.sh`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo doc --workspace --no-deps`

## Facade Example Surface

The public examples under `examples/` are the main end-to-end validation surface:

- `cargo test -p styx-examples --no-default-features --test smoke`
- `cargo run -p styx-examples --no-default-features --bin quickstart_capture_virtual`
- `cargo run -p styx-examples --no-default-features --features preview-window --bin low_latency_preview`
- `cargo run -p styx-examples --no-default-features --bin latest_frame_fanout`
- `cargo run -p styx-examples --no-default-features --features file-backend --bin reliable_recording -- /tmp/styx-recordings 12`
- `cargo run -p styx-examples --no-default-features --features async --bin async_pipeline`
- `cargo run -p styx-examples --no-default-features --features "v4l2 graph-pipeline" --bin v4l2_hardware_bench`
- `cargo run -p styx-examples --no-default-features --features "netcam codec-jpeg-decoder preview-window" --bin netcam_capture`
- `cargo run -p styx-examples --no-default-features --features "file-backend preview-window" --bin file_replay`

These examples exercise the consumer-facing facade, capture layers, codec integrations, and optional adapters together.

## Docker Surface

- `cargo test -p styx --test docker_facade_examples -- --ignored --nocapture`

The Docker suite uses [`testing/docker/styx-facade.Dockerfile`](../testing/docker/styx-facade.Dockerfile) and validates the virtual-camera facade flow inside a container.

## Additional Coverage

- Backend-specific validation should stay feature-gated and close to the example surface
- File-size linting fails for new non-baselined files above the release limit, supports `FILE_SIZE_EXCLUDE_DIRS=path1:path2`, and tracks current exceptions through `testing/ci/file-size-baseline.txt`
- Performance microbenchmarks live outside the default test surface; run `cargo bench -p styx --bench v4l2_capture_paths` when working on V4L2 capture-path performance
- CI now runs `./scripts/check-perf-smoke.sh`, which compares decode, transform, file replay, and mozjpeg encode p95 timings against `testing/perf/baseline.txt`

## Runtime Debugging

For stalls, frame drops, and pipeline closure, prefer a periodic `MediaPipeline::health_report()` log
and a `tracing` subscriber:

- Queue pressure: `capture_queue_depth`, `capture_queue_capacity`, and `capture_backpressure_count`.
- Drops: `drop_count` plus `drop_reasons`, especially `capture_queue_send_timeout`.
- Stage failures: `recent_stage_errors` in the health report or `MediaPipeline::last_stage_error()`.
- Copy pressure: `copy_count`, `bytes_moved`, and `recent_residency_transitions`.
- Async worker behavior: `capture_async_send_waits`, `capture_async_recv_waits`, and tracing fields
  `worker`, `receive`, and `processing`.
