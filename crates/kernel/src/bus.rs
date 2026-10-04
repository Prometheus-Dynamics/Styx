//! The Styx sensor bridge: the userspace side of `kernel-modules/styx-sensor-bridge`
//! (`PROTOCOL.md`): pad format, timing controls, power, stream start/stop handshake.
//!
//! The generic buses a userspace sensor driver also needs (I²C over i2c-dev, GPIO over the
//! character device, embedded-hal over them, kernel uevents) are Lemnos's
//! (`lemnos_linux::hal`, `lemnos_linux::uevent`).

pub mod bridge;
mod bridge_sys;

pub use bridge::{
    BridgeLocation, PadFormat, SensorBridge, StreamAction, StreamRequest, StreamState, Timing,
    find_bridges,
};
