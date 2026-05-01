# Release Review Tasks

Rust release-readiness checklist for the 2.0.0 review. Non-Rust FFI internals are out of scope.

## Completed

- [x] Audit Rust API ergonomics and add simple high-level capture/pipeline entry points.
- [x] Review public preludes and focused import surfaces.
- [x] Add explicit graph versus linear pipeline execution selection.
- [x] Audit backend worker loops for explicit stop, bounded sleeps, and shutdown behavior.
- [x] Ensure async capture drop does not block Tokio runtime workers.
- [x] Surface pipeline worker terminal failures through results and service events.
- [x] Add capture retry and shutdown health telemetry.
- [x] Make codec registry limits runtime configurable.
- [x] Make netcam MJPEG maximum frame size runtime configurable.
- [x] Fix async netcam reconnect/backoff behavior for ended streams.
- [x] Validate bounded queue wake and cancellation behavior under contention.
- [x] Add FourCC helpers for raw Bayer format classification.
- [x] Review shared buffer pool sizing and lifecycle for high-resolution pipelines.
- [x] Collapse duplicate sync/async MJPEG parsing logic into a shared parser/emitter state machine.
- [x] Extract common codec descriptor and shared-output boilerplate for raw decoders.
- [x] Review codec implementation identifiers for typed use at all selection boundaries.
- [x] Review backend and service event string identifiers for stable typed enums.
- [x] Review file sizes against the 800-line release target.
- [x] Review lint suppressions and dependency feature gating.
- [x] Expand release feature-combination and performance smoke checks.

## Final Verification

- [x] `cargo fmt --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace --all-targets`
- [x] `bash scripts/check-feature-combinations.sh`
- [x] `./scripts/check-file-sizes.sh`
