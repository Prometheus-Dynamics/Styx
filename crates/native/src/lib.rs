//! The Styx native camera runtime: camera sensors driven from userspace Rust over I²C, with the
//! generic Styx sensor bridge standing in for a kernel sensor driver.
//!
//! See `docs/native-stack/README.md` and `kernel-modules/styx-sensor-bridge/PROTOCOL.md`.
//!
//! # Lifecycle and faults
//!
//! [`NativeCamera::open`] is exclusive (an advisory lock on the bridge; a second open fails with
//! [`NativeError::Busy`]) and starts from a powered-down sensor. [`NativeCamera::configure`]
//! powers it and sets the mode, [`NativeCamera::start`] returns a [`FrameStream`],
//! [`NativeCamera::stop`] ends it, and dropping or closing the camera puts the sensor in standby,
//! powers it down and switches the bridge off, whatever failed before.
//!
//! - Frames outlive their stream: stopping releases the capture queue's buffers at once (held
//!   frames keep their memory until dropped and never go back to a newer queue), so a restart
//!   or a reopen gets fresh buffers while old frames are still held.
//! - A failed start (the bridge's acknowledgement timed out, the sensor did not answer on I²C)
//!   leaves the camera configured with the sensor in standby; its error says which.
//! - A running stream ends after one error item when the bridge or the capture node goes away
//!   ([`NativeError::Disconnected`], see [`NativeError::is_disconnect`]), when the receiver
//!   fails, when the sensor stops taking control writes, or when
//!   [`CameraOptions::max_error_frames`] frames in a row arrive corrupted. Missing frame-start
//!   events are replaced by the dequeues ([`FrameStream::frame_sync_fallback`]).
//! - Styx's capture supervisor reconnects native captures like V4L2 and libcamera ones.
//!
//! # Async
//!
//! [`FrameStream`] is a `futures_core::Stream` that works on any executor or none
//! ([`FrameStream::next_blocking`]): the camera's event thread polls the capture node and wakes
//! the waiting task. Cancelling a wait loses no frame. See `examples/async_frames.rs`.
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
pub mod external;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod fake_bridge;
#[cfg(test)]
mod fault_tests;
pub mod formats;
pub mod graph;
mod health;
pub mod kernel;
#[cfg(test)]
mod kernel_tests;
pub mod lens;
pub mod library;
pub mod modes;
pub mod provider;
pub mod regbus;
pub mod sensor_bus;
mod session;
mod stream;
pub mod topology;

pub use buffers::{BufferMemory, NativeFrame};
pub use camera::{
    CameraControls, CameraOptions, Configured, NativeCamera, StreamSettings, select_mode,
};
pub use control::{BringUpTimes, ControlHandle, FrameControls, SensorControl};
pub use discover::{CameraInfo, discover, discover_bridge};
pub use error::{NativeError, Result};
pub use external::SensorStream;
pub use kernel::{KernelSensor, discover_kernel};
pub use lens::{LensActuator, LensControl, LensInfo, LensKind};
pub use library::SensorLibrary;
pub use modes::SensorMode;
pub use provider::{NativeDevice, NativeProvider, PROVIDER_NAME};
pub use stream::{FrameStream, StreamStats};
pub use styx_graph::Fraction;
pub use styx_kernel::FourCc as KernelFourCc;
pub use styx_sensor;
