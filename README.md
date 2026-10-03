# Styx

Styx is a Rust workspace for sync-first, zero-copy media pipelines. The facade crate keeps capture, decode, transform, encode, graph, watch, service, and recording workflows behind one API while the support crates remain independently usable.

## Quick Start

```toml
[dependencies]
styx = { version = "2.0.0", default-features = false, features = ["native", "v4l2"] }
```

```rust
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wants = FrameRequirements::formats([FourCc::NV12])
        .output_resolution(1280, 800)
        .min_fps(30)
        .priority(Priority::Power); // exactly 30 fps where the camera can
    let plan = styx::planner::plan_best(&styx::probe_all(), &wants)?;
    println!("{plan}"); // camera, mode, every step and its cost, rejected options
    for frame in plan.start()?.take(90) {
        let meta = frame.meta(); // timestamp, sequence, exposure and gain that made it
        println!("{} {} {:?}", meta.timestamp, meta.format.code, meta.native());
    }
    Ok(())
}
```

The same code takes a USB camera (V4L2, YUYV converted), a CSI sensor Styx drives itself (the
Raspberry Pi PiSP or a software ISP, 3A in Rust) or, with the `libcamera` feature, a libcamera
camera. More: [examples/](examples/README.md) (listing cameras, async capture, controls and
per-frame metadata, two consumers of one camera, other processes, raw frames and recordings,
hotplug, adding a camera), and [docs/comparison.md](docs/comparison.md), the same tasks in
libcamera, V4L2 and GStreamer, with measurements.

## Workspace Layout

- `crates/styx`: facade crate for capture requests, pipeline sessions, graph integration, service events, watch runtime, and backend probing.
- `crates/core`: pooled buffers, formats, queues, controls, and low-level media primitives.
- `crates/capture`: capture descriptors, validation, `CaptureSource`, and virtual capture helpers.
- `crates/codec`: codec traits, registry, MJPEG/raw decoding, and optional FFmpeg/JPEG integrations.
- `crates/v4l2`, `crates/kernel`: the V4L2 backend (UVC and other V4L2 cameras) on Styx's own
  kernel interfaces (`libc` only).
- `crates/native`, `crates/sensor`, `crates/pipeline`, `crates/pisp`, `crates/softisp`,
  `crates/algo`, `crates/graph`: the native stack: sensors driven from Rust through the Styx
  sensor bridge, the Raspberry Pi PiSP or a software ISP, 3A in Rust
  (`docs/native-stack/README.md`). No libcamera.
- `crates/libcamera`: optional compat backend for cameras the native stack does not cover; never
  needed by the other crates.
- `crates/gst-styx`, `crates/pipewire-styx`: optional GStreamer plugin (`styxsrc`) and PipeWire camera node; separate workspaces that need the GStreamer/PipeWire development files, not built by the root workspace (see [docs/ecosystem.md](docs/ecosystem.md)).
- `examples`: top-level example package grouped by quickstart, capture, graph, codec, performance, and app workflows.
- `testing`: Docker, perf, and CI baselines used by local and hosted validation.

## Getting Started

Styx currently requires Rust 1.94.0. The workspace pins this toolchain in
`rust-toolchain.toml` and mirrors it as `rust-version = "1.94"` in
`Cargo.toml` so release builds, docs, examples, and CI use the same compiler.

Add the facade crate:

```toml
[dependencies]
styx = "2.0.0"
```

Useful example entry points:

- `cargo run -p styx-examples --no-default-features --bin quickstart_capture_virtual`
- `cargo run -p styx-examples --no-default-features --features preview-window --bin low_latency_preview`
- `cargo run -p styx-examples --no-default-features --features file-backend --bin reliable_recording -- /tmp/styx-recordings 30`
- `cargo run -p styx-examples --no-default-features --bin latest_frame_fanout`
- `cargo run -p styx-examples --no-default-features --features async --bin async_pipeline`
- `cargo run -p styx-examples --no-default-features --features "v4l2 graph-pipeline" --bin v4l2_hardware_bench`
- `cargo run -p styx-examples --no-default-features --features "netcam preview-window" --bin netcam_capture -- http://cam/mjpeg`
- `cargo run -p styx-examples --no-default-features --features "file-backend preview-window" --bin file_replay -- frame1.png frame2.png`
- `cargo run -p styx-examples --no-default-features --features "libcamera codec-ffmpeg preview-window" --bin libcamera_ffmpeg_preview --release`

## Recommended API Paths

Use `styx::prelude::*` when prototyping across capture, codec, graph, and service APIs. For
production code that only needs one area, prefer the task-focused imports:
`styx::imports::framelease::*`, `styx::imports::capture::*`, `styx::imports::pipeline::*`,
`styx::imports::codec::*`, `styx::imports::service::*`, and `styx::imports::watch::*`.
Feature-gated modules such as
`styx::imports::graph::*` and `styx::imports::recording::*` are available with their matching
features.

Crates that only need `FrameLease` can depend on Styx without capture or codec:
```toml
styx = { version = "2.0.0", default-features = false, features = ["framelease"] }
```

For simple capture, start with `CameraRequest::new().start()` when physical backends are enabled, or
`CaptureRequest::virtual_source(...)` for deterministic local/test capture. Receive frames
with `recv`, `recv_blocking`, `recv_timeout`, `recv_forever`, or `recv_async`; stop handles explicitly
with `stop` or `stop_async`.

For decode/process/record workflows, prefer `MediaPipelineBuilder` over wiring queues manually. The
pipeline owns capture startup, optional decode/transform hooks, service events, health reports, and
recording sinks while keeping frame payloads as `FrameLease` values.

The `async` feature provides Tokio-backed receive/control helpers and async netcam workers. Most
camera backends are still sync-first worker-thread integrations because the underlying camera APIs
are synchronous. In async services, run slow startup or CPU-heavy pipeline workers on an application
blocking task/thread, then use async receive/control APIs for coordination.

## Features

- `hooks`: `FrameLease` hook and recording support inside the pipeline without pulling the `image` crate.
- `image`: DynamicImage conversion helpers and PNG/JPEG image-crate recording fallback for raw frames.
- `async`: Tokio-backed async capture and pipeline helpers.
  Use `MediaPipeline::spawn_tokio_worker` or `spawn_blocking_worker` for CPU-heavy decode,
  encode, graph, hook, or sink pipelines; `next_async` keeps receive async but processes frames
  synchronously on the calling task. Capture startup is also sync-first;
  use an application blocking task/thread for slow camera startup in async services.
  Use `CaptureHandle::stop_async` or `stop_async_in_place` when explicit teardown matters in
  Tokio code.
- `preview-window`: Minifb preview window support for examples.
- `raw-decoders`: CPU raw/YUV/Bayer conversion stack; this enables Rayon and `yuvutils-rs`.
- `codec-jpeg-decoder`: pure-Rust MJPEG/JPEG decode via `jpeg-decoder`.
- `codec-ffmpeg`, `codec-mozjpeg`, `codec-turbojpeg`, `codec-zune`: alternate codec integrations; FFmpeg and native JPEG backends add their respective system/library dependency surface.
- `netcam-video`: FFmpeg-backed network video stream fallback; use only when multipart MJPEG is not enough.
- `simulation-bevy`: Bevy/wgpu-based synthetic capture; useful for simulation, but intentionally kept out of default builds.
- `v4l2`: V4L2 cameras (UVC and other kernel-driven cameras), on `libc` only.
- `native`: sensors Styx drives itself through the Styx sensor bridge, processed by the PiSP or
  the software ISP (`docs/native-stack/README.md`); no C dependencies.
- `libcamera`: optional compat backend through libcamera (needs its headers and library at
  build time); `v4l2` and `native` probe and plan without it. It enables `raw-decoders` for
  format emulation.
- `netcam`: Reqwest-backed network camera capture; combine with `async` for async workers.
- `netcam-video`: FFmpeg-backed fallback for container/video netcam streams.
- `file-backend`, `file-backend-video`: disk-backed replay sources; video replay enables FFmpeg.
- `graph-pipeline`: Daedalus-backed graph execution, edge policies, and graph telemetry.
- `simulation-bevy`: Bevy-backed synthetic scene capture and the heaviest optional feature group.
- `schema`, `serde`: API schema and serialization support.
- `examples`: convenience bundle for example-oriented features.

## Development

Common workspace commands:

```bash
./scripts/repo-clean.sh
./scripts/ci.sh
cargo fmt -p styx-core-rs -p styx-capture -p styx-codec -p styx-libcamera -p styx -p styx-v4l2 -p styx-examples -- --check
./scripts/check-file-sizes.sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
bash ./scripts/check-feature-combinations.sh
cargo tree -d --workspace --no-default-features
cargo doc --workspace --no-deps
```

Perf-smoke commands:

```bash
cargo run -p styx-examples --no-default-features --features codec-jpeg-decoder --bin perf_smoke --release --quiet
cargo run -p styx-examples --no-default-features --features file-backend --bin file_replay_perf --quiet
cargo run -p styx-examples --no-default-features --features codec-mozjpeg --bin encode_perf --quiet
```

Optional Docker-backed facade validation:

- `cargo test -p styx --test docker_facade_examples -- --ignored --nocapture`

## Documentation Index

- [docs/comparison.md](docs/comparison.md): Styx compared with libcamera, raw V4L2 and GStreamer: code side by side, architecture, measurements
- [examples/README.md](examples/README.md): runnable examples, with output from the CM5
- [docs/native-stack/README.md](docs/native-stack/README.md): the native camera stack (sensors, PiSP, 3A, sensor bridge)
- [docs/README.md](docs/README.md): repository documentation index
- [docs/ecosystem.md](docs/ecosystem.md): GStreamer and PipeWire bridges
- [docs/development.md](docs/development.md): repo layout, commands, and validation conventions
- [docs/testing.md](docs/testing.md): test surfaces, example expectations, and CI notes
- [CHANGELOG.md](CHANGELOG.md): release history and notable workspace changes
- [testing/README.md](testing/README.md): local and CI validation entry points
- [scripts/ci.sh](scripts/ci.sh): shared local CI entry point
- [scripts/repo-clean.sh](scripts/repo-clean.sh): pre-commit cleanup and verification entry point
- [crates/styx/README.md](crates/styx/README.md): facade API notes
- [crates/core/README.md](crates/core/README.md): core primitives
- [crates/capture/README.md](crates/capture/README.md): capture traits and descriptors
- [crates/codec/README.md](crates/codec/README.md): codec registry and integrations

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License
