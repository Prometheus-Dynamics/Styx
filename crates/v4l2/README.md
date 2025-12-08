# styx-v4l2

V4L2 probing backend for Styx. This crate scans `/dev/video*` nodes, filters
non-camera endpoints, and emits `CaptureDescriptor` entries with available
formats, intervals, and controls.

## Documentation
- <https://docs.rs/styx-v4l2>

## Install
```toml
[dependencies]
styx-v4l2 = "0.1.0"
```

## Usage
Enable the `v4l2` feature on the `styx` crate to access probing helpers:
```rust,ignore
use styx::prelude::*;

let devices = probe_all();
for device in devices {
    println!("{} backends: {}", device.identity.name, device.backends.len());
}
```
