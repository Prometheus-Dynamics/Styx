# Testing

Styx splits validation into default workspace checks and facade-example coverage.

## Default Surface

The gate before merging to `dev` is `./scripts/gate.sh` ([development.md](development.md#the-gate)).

- `cargo fmt --all --check`
- `./scripts/check-file-sizes.sh`
- `./scripts/check-test-targets.sh` (binaries marked `test = false` hold no tests; one
  integration-test binary per package besides the listed exceptions)
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo doc --workspace --no-deps`

## Integration Tests

Each package's integration tests are one test binary, `tests/it/main.rs`, with a module per
topic (`mod sim_ae;`, `mod camera_service;`), so an edit relinks one binary per package
([development.md](development.md#the-gate)). New tests go into a module there, not into a new
`tests/*.rs` file:

- Shared helpers live in `tests/it/common/` (declared once in `main.rs`; the modules
  `use crate::common;`).
- A module that needs a feature or a platform is gated on its `mod` line
  (`#[cfg(all(target_os = "linux", feature = "preview"))] mod preview;`), not with `#![cfg]` or
  a `required-features` entry.
- `cargo test` runs a binary's tests in threads of one process (nextest runs each test in its own
  process). A test that touches process-wide state (a global pool or registry, environment
  variables, the working directory) serialises with the tests it could disturb through a static
  `Mutex`, or, when that is not enough, gets a binary of its own: a test that installs a
  `#[global_allocator]`, or one that checks process-wide counts. Such a binary is listed with its
  reason in `scripts/check-test-targets.sh`, which fails on any other second binary.
- Run one module's tests with a filter: `cargo test -p styx --test it camera_service`.

## Facade Example Surface

The public examples under `examples/` are the main end-to-end validation surface:

- `cargo test -p styx-examples --no-default-features --test smoke`
- `cargo run -p styx-examples --no-default-features --bin quickstart_capture_virtual`
- `cargo run -p styx-examples --no-default-features --features preview-window --bin low_latency_preview`
- `cargo run -p styx-examples --no-default-features --bin latest_frame_fanout`
- `cargo run -p styx-examples --no-default-features --features file-backend --bin reliable_recording -- /tmp/styx-recordings 12`
- `cargo run -p styx-examples --no-default-features --features async --bin async_pipeline`
- `cargo run -p styx-examples --no-default-features --features v4l2 --bin v4l2_hardware_bench`
- `cargo run -p styx-examples --no-default-features --features "netcam codec-jpeg-decoder preview-window" --bin netcam_capture`
- `cargo run -p styx-examples --no-default-features --features "file-backend preview-window" --bin file_replay`

These examples exercise the consumer-facing facade, capture layers, codec integrations, and optional adapters together.

## Docker Surface

- `cargo test -p styx --test it docker_facade_examples -- --ignored --nocapture`

The Docker suite uses [`testing/docker/styx-facade.Dockerfile`](../testing/docker/styx-facade.Dockerfile) and validates the virtual-camera facade flow inside a container.

## Additional Coverage

- Backend-specific validation should stay feature-gated and close to the example surface
- File-size linting fails for new non-baselined files above the release limit, supports `FILE_SIZE_EXCLUDE_DIRS=path1:path2`, and tracks current exceptions through `testing/ci/file-size-baseline.txt`
- Performance microbenchmarks live outside the default test surface; run `cargo bench -p styx --bench v4l2_capture_paths` when working on V4L2 capture-path performance
- CI now runs `./scripts/check-perf-smoke.sh`, which compares decode, transform, file replay, and mozjpeg encode p95 timings against `testing/perf/baseline.txt`
- CI runs `./scripts/check-mem-smoke.sh`, which checks heap and resident memory of capture and decode scenarios against `testing/perf/memory-baseline.txt` and fails on leaks (see [performance.md](performance.md#memory-smoke-surface))
- Damaged input: `crates/styx/src/replay/corruption_tests.rs` (truncated and corrupted MCAP/`.styxrec` recordings, with an allocation bound), `crates/codec/src/corruption_tests.rs` (damaged MJPEG through every decoder) and the netcam multipart parser tests run in CI; the cargo-fuzz targets ([fuzzing.md](fuzzing.md)) cover these and every other parser of untrusted bytes for longer runs

## Runtime Debugging

For stalls, frame drops, and pipeline closure, prefer a periodic `MediaPipeline::health_report()` log
and a `tracing` subscriber:

- Queue pressure: `capture_queue_depth`, `capture_queue_capacity`, and `capture_backpressure_count`.
- Drops: `drop_count` plus `drop_reasons`, especially `capture_queue_send_timeout`.
- Stage failures: `recent_stage_errors` in the health report or `MediaPipeline::last_stage_error()`.
- Copy pressure: `copy_count`, `bytes_moved`, and `recent_residency_transitions`.
- Async worker behavior: `capture_async_send_waits`, `capture_async_recv_waits`, and tracing fields
  `worker`, `receive`, and `processing`.

## Image quality comparisons

`tools/iq-compare` (crate `styx-iq-compare`) puts numbers on two raw grey (Y8) captures of the same
scene: frames back to back, no padding (the layout of `styx-record` and the libcamera recorder's
`.raw` files). It needs no dependencies and streams the frames.

```sh
cargo run -p styx-iq-compare -- \
  /home/sozo/styx-tag-frames-2026-10-12/frames_1280x800_gray.raw \
  /home/sozo/libcamera-tag-frames-2026-10-12/frames_1280x800_gray.raw
```

Options: `--width`, `--height` (default 1280x800), `--skip N` and `--frames N` to select frames,
`--noise-roi` (default `560,120,760,300`, the static wall patch) and `--edge-roi` (default
`622,180,658,222`, AprilTag 5). Each file is one column. The measures:

* temporal std: per-pixel sample standard deviation over frames on the noise patch (mean, median,
  90th percentile), and the per-frame patch mean's std (`frame mean std`, global brightness change
  such as light flicker);
* spatial sigma: RMS of `x - mean(4 neighbours)` on one frame, the largest 1% left out, divided by
  `sqrt(1.25)`, averaged over frames (includes static texture: use it on a flat patch);
* edge: 10th and 90th percentiles of the edge region's mean image, their difference, and the
  central-difference gradient energy (mean of gx² + gy²).

Compare captures at matched exposure and gain: a gain difference alone changes the noise. With
the default regions the Styx capture (7998 µs, 7.5x) gives temporal std 1.68 (median 0.71) and
the libcamera capture (AE, 29993 µs, 1.5x) 0.33, so the two are not a like-for-like noise test.
