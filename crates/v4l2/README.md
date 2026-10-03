# styx-v4l2

V4L2 probing backend for Styx. This crate scans `/dev/video*` nodes, skips
non-camera endpoints, and emits `CaptureDescriptor` entries with available
formats, intervals, and controls.

Nodes that are not cameras (decoders, ISP and receiver nodes, statistics, configuration and
embedded data nodes, UVC metadata nodes) and nodes unplugged while probing are skipped quietly:
they are logged at `debug` level (`tracing`), not returned as errors. The errors are real
failures only, such as a node that cannot be opened.

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

Virtual nodes (v4l2loopback, "OBS Virtual Camera") are skipped unless
`STYX_V4L2_ALLOW_VIRTUAL=1` is set, which is handy for testing with a loopback device fed by
another program.
