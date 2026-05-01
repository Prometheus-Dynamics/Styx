# Documentation

This directory holds repository-level documentation for the Styx workspace.

## Guides

- [development.md](development.md): repository layout, validation commands, and contribution expectations
- [api-ergonomics.md](api-ergonomics.md): import surfaces, common task recipes, and typed API boundaries
- [performance.md](performance.md): benchmark surfaces and performance-validation notes
- [runtime-debugging.md](runtime-debugging.md): runtime tracing, health, queue, and teardown diagnostics
- [testing.md](testing.md): default and example-oriented validation surfaces

## Where To Start

- Using Styx: start with the root [README.md](../README.md) and [`crates/styx/README.md`](../crates/styx/README.md)
- Narrow facade imports: use the task modules documented in the root [README.md](../README.md#recommended-api-paths)
- Core media primitives: read [`crates/core/README.md`](../crates/core/README.md)
- Capture layers: read [`crates/capture/README.md`](../crates/capture/README.md), [`crates/libcamera/README.md`](../crates/libcamera/README.md), and [`crates/v4l2/README.md`](../crates/v4l2/README.md)
- Codec integrations: read [`crates/codec/README.md`](../crates/codec/README.md)
- Running validation: read [testing.md](testing.md) and [`../testing/README.md`](../testing/README.md)
