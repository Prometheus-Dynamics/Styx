# Documentation

This directory holds repository-level documentation for the Styx workspace.

## Guides

- [comparison.md](comparison.md): Styx compared with libcamera, raw V4L2 and GStreamer: the same tasks in code, architecture, measured numbers, what Styx does not do yet
- [native-stack/README.md](native-stack/README.md): the native camera stack (sensor bridge, sensors as data, PiSP, 3A in Rust)
- [development.md](development.md): repository layout, validation commands, and contribution expectations
- [api-ergonomics.md](api-ergonomics.md): import surfaces, common task recipes, and typed API boundaries
- [frame-planning.md](frame-planning.md): describing the frames a consumer needs and letting Styx plan capture, decode, output size, pyramids and ROI
- [frame-server.md](frame-server.md): sharing camera frames with other processes without copying
- [ecosystem.md](ecosystem.md): Styx cameras in GStreamer (`styxsrc`) and PipeWire (optional bridge crates)
- [encoding.md](encoding.md): FFmpeg encoders, low-latency defaults, and handing camera buffers to encoders without copies
- [hw-frame-preparation.md](hw-frame-preparation.md): hardware frame-preparation investigation and on-device results
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
