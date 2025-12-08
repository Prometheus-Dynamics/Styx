# styx-libcamera

Libcamera probing backend for Styx. This crate builds `CaptureDescriptor` entries
from libcamera devices and exposes their controls/format metadata to the Styx
capture API.

## Documentation
- <https://docs.rs/styx-libcamera>

## Install
```toml
[dependencies]
styx-libcamera = "0.1.0"
```

## Features
- `probe`: enable libcamera probing and descriptor construction.
- `vendor_rpi`: include Raspberry Pi vendor draft controls when probing.

## Usage
Enable the `libcamera` feature on the `styx` crate to access probing helpers:
```rust,ignore
use styx::prelude::*;

let devices = probe_all();
for device in devices {
    println!("{} backends: {}", device.identity.name, device.backends.len());
}
```
