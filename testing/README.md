# Testing

Styx splits validation into repo-local workspace checks, portable example smoke tests, optional Docker facade tests, and performance guardrails.

## Default Surface

- `./scripts/check-file-sizes.sh`
- `cargo fmt -p styx-core-rs -p styx-capture -p styx-codec -p styx-libcamera -p styx -p styx-v4l2 -p styx-examples -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `bash ./scripts/check-feature-combinations.sh`
- `cargo tree -d --workspace --no-default-features`
- `cargo test -p styx-examples --no-default-features --test smoke`
- `cargo check -p styx-examples --no-default-features --features "async,file-backend,netcam"`
- `./scripts/check-perf-smoke.sh`

## Docker Surface

- `cargo test -p styx --test docker_facade_examples -- --ignored --nocapture`

The Docker suite uses [`testing/docker/styx-facade.Dockerfile`](docker/styx-facade.Dockerfile) and validates virtual-camera facade examples inside a container.

## Additional Coverage

- Backend-specific validation should stay feature-gated and close to the example surface.
- The default GitHub Actions workflow runs formatting, file-size linting, workspace tests, portable example checks, perf smoke, clippy, docs, and package-surface checks.
- File-size linting is warning-only, supports `FILE_SIZE_EXCLUDE_DIRS=path1:path2`, and tracks existing oversized files through `testing/ci/file-size-baseline.txt`.
