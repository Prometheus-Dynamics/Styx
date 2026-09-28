//! The Styx native camera runtime: camera sensors driven from userspace Rust over I²C, with the
//! generic Styx sensor bridge standing in for a kernel sensor driver.
//!
//! See `docs/native-stack/README.md` and `kernel-modules/styx-sensor-bridge/PROTOCOL.md`.
#![deny(unsafe_code)]

mod buffers;
pub mod control;
mod error;
pub mod formats;
pub mod graph;
pub mod library;
pub mod modes;
pub mod regbus;
mod stream;
pub mod topology;

pub use buffers::{BufferMemory, NativeFrame};
pub use control::{ControlHandle, FrameControls, SensorControl};
pub use error::{NativeError, Result};
pub use library::SensorLibrary;
pub use modes::SensorMode;
pub use stream::{FrameStream, StreamStats};
