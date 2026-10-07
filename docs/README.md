# Documentation

This directory holds repository-level documentation for the Styx workspace.

## Guides

- [comparison.md](comparison.md): Styx compared with libcamera, raw V4L2 and GStreamer: the same tasks in code, architecture, measured numbers, what Styx does not do yet
- [native-stack/README.md](native-stack/README.md): the native camera stack (sensor bridge, sensors as data, PiSP, 3A in Rust)
- [portability.md](portability.md): the platform-neutral crates without `std` (3A, software ISP, sensors, PiSP config, DNG, formats) on microcontrollers, RTOSes and WebAssembly
- [mcu.md](mcu.md): Styx's footprint on microcontrollers: flash, static RAM and heap of four firmware configurations (Cortex-M7/M4F, Cortex-M0+), what fits on STM32H7/F4, RP2040 and ESP32-S3, the features and profile to use
- [portability-design.md](portability-design.md): design for review: `styx-hal` and a `no_std` camera runtime, Linux as the reference platform, MCU and rkisp1 port sketches, migration plan
- [development.md](development.md): repository layout, validation commands, and contribution expectations
- [api-ergonomics.md](api-ergonomics.md): import surfaces, common task recipes, and typed API boundaries
- [frame-planning.md](frame-planning.md): describing the frames a consumer needs and letting Styx plan capture, decode, output size, pyramids and ROI
- [stills-and-dng.md](stills-and-dng.md): stills from a running capture (PiSP / software ISP reprocessing, exposure brackets) and DNG raw files (`styx-dng` writer and reader)
- [frame-server.md](frame-server.md): sharing camera frames with other processes without copying
- [recording.md](recording.md): `styx-record`, grey test recordings (Y plane, Eidos raw video layout) with a timestamp/sequence sidecar and the camera settings, from a camera service or the camera directly; stills for calibration
- [preview.md](preview.md): camera previews for user interfaces: small JPEGs (MJPEG over HTTP, WebSocket messages) from a low-priority camera service client that never changes the vision consumer's capture; encoder benchmarks and the CM5 measurement plan
- [ecosystem.md](ecosystem.md): Styx cameras in GStreamer (`styxsrc`) and PipeWire (optional bridge crates)
- [encoding.md](encoding.md): FFmpeg encoders, low-latency defaults, and handing camera buffers to encoders without copies
- [hw-frame-preparation.md](hw-frame-preparation.md): hardware frame-preparation investigation and on-device results
- [performance.md](performance.md): benchmark surfaces and performance-validation notes
- [runtime-debugging.md](runtime-debugging.md): runtime tracing, health, queue, and teardown diagnostics
- [metrics.md](metrics.md): per-camera health and performance metrics (rates, drops by cause, latency, ISP/CPU time, 3A, consumers), Prometheus text and the camera service's metrics request
- [multi-camera-sync.md](multi-camera-sync.md): frames of several cameras grouped by sensor timestamp (`styx::multicam`), sync quality metrics, one Daedalus tick per group, and hardware frame sync (OV9782/OV9281 FSIN and strobe, what a CM5 carrier needs)
- [daedalus.md](daedalus.md): Styx frames in Daedalus graphs: the `daedalus` feature of `styx-core-rs` (frame type, descriptor, metadata adapter, zero-copy payloads) and how to enable it
- [testing.md](testing.md): default and example-oriented validation surfaces
- [fuzzing.md](fuzzing.md): the cargo-fuzz targets for every parser of untrusted bytes, how to run and add them

## Where To Start

- Using Styx: start with the root [README.md](../README.md) and [`crates/styx/README.md`](../crates/styx/README.md)
- Narrow facade imports: use the task modules documented in the root [README.md](../README.md#recommended-api-paths)
- Core media primitives: read [`crates/core/README.md`](../crates/core/README.md)
- Capture layers: read [`crates/capture/README.md`](../crates/capture/README.md), [`crates/libcamera/README.md`](../crates/libcamera/README.md), and [`crates/v4l2/README.md`](../crates/v4l2/README.md)
- Codec integrations: read [`crates/codec/README.md`](../crates/codec/README.md)
- Running validation: read [testing.md](testing.md) and [`../testing/README.md`](../testing/README.md)
