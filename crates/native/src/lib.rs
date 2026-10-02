//! The Styx native camera runtime: camera sensors driven from userspace Rust over I²C, with the
//! generic Styx sensor bridge standing in for a kernel sensor driver.
//!
//! See `docs/native-stack/README.md` and `kernel-modules/styx-sensor-bridge/PROTOCOL.md`.
#![deny(unsafe_code)]

#[cfg(test)]
mod async_tests;
mod buffers;
pub mod camera;
pub mod control;
mod device;
pub mod discover;
mod embedded;
mod error;
mod events;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod fake_bridge;
#[cfg(test)]
mod fault_tests;
pub mod formats;
pub mod graph;
mod health;
pub mod library;
pub mod modes;
pub mod provider;
pub mod regbus;
mod session;
mod stream;
pub mod topology;

pub use buffers::{BufferMemory, NativeFrame};
pub use camera::{
    CameraControls, CameraOptions, Configured, NativeCamera, StreamSettings, select_mode,
};
pub use control::{ControlHandle, FrameControls, SensorControl};
pub use discover::{CameraInfo, discover, discover_bridge};
pub use error::{NativeError, Result};
pub use library::SensorLibrary;
pub use modes::SensorMode;
pub use provider::{NativeDevice, NativeProvider, PROVIDER_NAME};
pub use stream::{FrameStream, StreamStats};
pub use styx_graph::Fraction;
pub use styx_kernel::FourCc as KernelFourCc;
pub use styx_sensor;
