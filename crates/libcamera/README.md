# styx-libcamera

Libcamera probing backend for Styx. This crate builds `CaptureDescriptor` entries
from libcamera devices and exposes their controls/format metadata to the Styx
capture API.

## Documentation
- <https://docs.rs/styx-libcamera>

## Install
```toml
[dependencies]
styx-libcamera = "2.0.0"
```

## Features
- `probe`: enable libcamera probing and descriptor construction.
- `vendor_rpi`: kept for compatibility, does nothing (Raspberry Pi vendor controls are not built; upstream generation is broken in libcamera 0.7).

## Usage
Enable the `libcamera` feature on the `styx` crate to access probing helpers:
```rust
#[cfg(feature = "probe")]
{
use styx_libcamera::probe_devices_with_errors;

let (devices, errors) = probe_devices_with_errors();
for device in devices {
    println!("{} modes: {}", device.id, device.descriptor.modes.len());
}
assert!(errors.iter().all(|err| !err.is_empty()));
}
```
