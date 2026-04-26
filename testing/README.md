# Testing

Styx splits validation into default workspace checks and facade-example coverage.

## Default Surface

- `./scripts/check-file-sizes.sh`
- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo run -p styx --example capture_virtual`
- `cargo run -p styx --example low_latency_preview --features preview-window`

## Docker Surface

- `cargo test -p styx --test docker_facade_examples -- --ignored --nocapture`

The Docker suite uses [`testing/docker/styx-facade.Dockerfile`](docker/styx-facade.Dockerfile) and validates virtual-camera facade examples inside a container.

## Additional Coverage

- Backend-specific validation should stay feature-gated and close to the example surface.
- The default GitHub Actions workflow runs formatting, workspace tests, linting, documentation, and package-surface checks.
- File-size linting is warning-only, supports `FILE_SIZE_EXCLUDE_DIRS=path1:path2`, and tracks existing oversized files through `testing/ci/file-size-baseline.txt`.
