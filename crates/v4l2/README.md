# styx-v4l2

V4L2 probing backend for Styx. This crate scans `/dev/video*` nodes, filters
non-camera endpoints, and emits `CaptureDescriptor` entries with available
formats, intervals, and controls.

## Documentation
- <https://docs.rs/styx-v4l2>

## Install
```toml
[dependencies]
styx-v4l2 = "2.0.0"
```

## Usage
Enable the `v4l2` feature on the `styx` crate to access probing helpers:
```rust
use styx_v4l2::probe_devices;

let (devices, errors) = probe_devices();
for device in devices {
    println!("{} modes: {}", device.path, device.descriptor.modes.len());
}
assert!(errors.iter().all(|err| !err.is_empty()));
```
