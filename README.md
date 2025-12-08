# Styx (sync-first, zero-copy media stack)

Styx is a layered Rust stack for building capture→decode→process→encode pipelines with zero-copy frames and predictable backpressure. The workspace is organized into small crates (core primitives, capture descriptors/backends, codecs/registry) and a facade crate `styx` that re-exports everything end users need.

## Documentation
- Crate docs: <https://docs.rs/styx>
- Examples live in `crates/styx/examples` and are feature-gated; see the quick start commands below.

## Install
```toml
[dependencies]
styx = "0.1.0"
```

## Crate map
- `crates/styx` — facade for end users; exports the prelude, capture API, pipeline builder, metrics, and backend helpers.
- `crates/core` — buffer pools, frame layouts, bounded/newest queues, FourCc/format types, controls, and simple metrics counters.
- `crates/capture` — descriptors/config validation, the `CaptureSource` trait, virtual capture backend helpers, and pool-based frame builders.
- `crates/codec` — unified `Codec` trait, codec registry, MJPEG/raw decoders, and feature-gated FFmpeg/mozjpeg/turbojpeg/zune integrations.
- Optional backends: `crates/libcamera` and `crates/v4l2` enable probing/streaming from system devices (behind features on the `styx` crate).

## Features (enable on the `styx` crate)
- `hooks` (default): enable image/frame hooks inside the pipeline.
- `async`: async capture/pipeline helpers.
- `preview-window`: simple minifb preview window for examples.
- `codec-ffmpeg`: FFmpeg encoders/decoders (H264/H265/MJPEG) via `ffmpeg-next`.
- `codec-mozjpeg`, `codec-turbojpeg`, `codec-zune`: alternate JPEG implementations.
- `v4l2`, `libcamera`: probe/stream physical devices.
- `netcam`, `netcam-video`: MJPEG-over-HTTP or FFmpeg streaming for network cameras.
- `file-backend`, `file-backend-video`: replay stills or videos as a capture source.
- `examples`: convenience bundle for example features (`preview-window`, `async`).

## Quick start
Virtual capture with metrics (no extra features needed):
```bash
cargo run -p styx --example capture_virtual
```

Capture → decode pipeline with hooks and optional preview window:
```bash
cargo run -p styx --example capture_and_decode --features preview-window
```

Async pipeline (enable `async`):
```bash
cargo run -p styx --example async_pipeline --features async
```

Netcam MJPEG pipeline (enable `netcam`, optionally `preview-window`):
```bash
cargo run -p styx --example netcam_capture --features "netcam preview-window" -- http://cam/mjpeg
```

File replay backend (enable `file-backend`):
```bash
cargo run -p styx --example file_replay --features "file-backend preview-window" -- frame1.png frame2.png
```

Libcamera → FFmpeg decode/encode preview (requires `libcamera`, `codec-ffmpeg`, `preview-window`; camera must expose MJPG/H264/H265):
```bash
cargo run -p styx --example libcamera_ffmpeg_preview --features "libcamera codec-ffmpeg preview-window" --release
```

## Examples (all live under `crates/styx/examples`)
- `capture_virtual`: tune `StyxConfig`, stream virtual frames, and print metrics.
- `capture_and_decode`: virtual capture into a pipeline with frame hooks and optional preview.
- `async_pipeline`: async variant of the pipeline.
- `mjpeg_decode`: register MJPEG decoder and run through the codec registry (embedded sample).
- `netcam_capture`: MJPEG netcam device builder + decode pipeline.
- `file_replay`: replay images/videos as frames with control overrides.
- `probe_and_select`: probe v4l2/libcamera devices and stream the first available.
- `libcamera_ffmpeg_preview`: libcamera capture with FFmpeg decode/encode and a preview window (requires MJPG/H264/H265 support).

## Development notes
- Everything exposed to end users is re-exported from `styx::prelude` (plus a few top-level types like `BackendHandle/BackendKind` and probing helpers).
- Buffers are pooled (`BufferPool`/`FrameLease`) to avoid allocations; use the provided layout helpers to honor strides and zero-copy constraints.
- Capture backends push frames over bounded queues; configure depth/pool sizes via `StyxConfig`/`set_capture_tunables`.
- Codecs are registered in `CodecRegistry`; selection is configurable via policies, impl priorities, and hardware bias flags.

See the per-crate READMEs in `crates/core`, `crates/capture`, `crates/codec`, and `crates/styx` for deeper API notes.

## License
Licensed under either of:
- Apache License, Version 2.0
- MIT License
