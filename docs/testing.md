# Testing

Styx splits validation into default workspace checks and facade-example coverage.

## Default Surface

- `cargo fmt --all -- --check`
- `./scripts/check-file-sizes.sh`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo doc --workspace --no-deps`

## Facade Example Surface

The public examples under `crates/styx/examples` are the main end-to-end validation surface:

- `cargo run -p styx --example capture_virtual`
- `cargo run -p styx --example capture_and_decode --features preview-window`
- `cargo run -p styx --example async_pipeline --features async`
- `cargo run -p styx --example netcam_capture --features "netcam preview-window"`
- `cargo run -p styx --example file_replay --features "file-backend preview-window"`

These examples exercise the consumer-facing facade, capture layers, codec integrations, and optional adapters together.

## Docker Surface

- `cargo test -p styx --test docker_facade_examples -- --ignored --nocapture`

The Docker suite uses [`testing/docker/styx-facade.Dockerfile`](../testing/docker/styx-facade.Dockerfile) and validates the virtual-camera facade flow inside a container.

## Additional Coverage

- Backend-specific validation should stay feature-gated and close to the example surface
- File-size linting is warning-only, supports `FILE_SIZE_EXCLUDE_DIRS=path1:path2`, and tracks current exceptions through `testing/ci/file-size-baseline.txt`
