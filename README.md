# Styx

Styx is a Rust workspace for sync-first, zero-copy media pipelines. The workspace is organized around a facade crate, focused support crates, feature-gated backend adapters, and example-driven validation.

## Workspace Layout

- `crates/styx`: facade crate for end users; re-exports the main capture, pipeline, hook, and backend surfaces.
- `crates/core`: pooled buffers, formats, queues, controls, and low-level media primitives.
- `crates/capture`: capture descriptors, validation, `CaptureSource`, and virtual capture helpers.
- `crates/codec`: codec traits, registry, MJPEG/raw decoding, and optional FFmpeg/JPEG integrations.
- `crates/libcamera`, `crates/v4l2`: optional system backends for probing and capture.

## Getting Started

Add the facade crate:

```toml
[dependencies]
styx = "1.0.0"
```

Useful example entry points:

- `cargo run -p styx --example capture_virtual`
- `cargo run -p styx --example capture_and_decode --features preview-window`
- `cargo run -p styx --example async_pipeline --features async`
- `cargo run -p styx --example netcam_capture --features "netcam preview-window" -- http://cam/mjpeg`
- `cargo run -p styx --example file_replay --features "file-backend preview-window" -- frame1.png frame2.png`
- `cargo run -p styx --example libcamera_ffmpeg_preview --features "libcamera codec-ffmpeg preview-window" --release`

## Features

- `hooks` (default): frame and image hook support inside the pipeline.
- `async`: async capture and pipeline helpers.
- `preview-window`: lightweight preview window support for examples.
- `codec-ffmpeg`, `codec-mozjpeg`, `codec-turbojpeg`, `codec-zune`: alternate codec integrations.
- `v4l2`, `libcamera`: physical capture backends.
- `netcam`, `netcam-video`: network camera capture.
- `file-backend`, `file-backend-video`: disk-backed replay sources.
- `examples`: convenience bundle for example-oriented features.

## Development

Common workspace commands:

```bash
./scripts/repo-clean.sh
cargo fmt --all -- --check
./scripts/check-file-sizes.sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
```

Optional Docker-backed facade validation:

- `cargo test -p styx --test docker_facade_examples -- --ignored --nocapture`

## Documentation Index

- [docs/README.md](docs/README.md): repository documentation index
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
