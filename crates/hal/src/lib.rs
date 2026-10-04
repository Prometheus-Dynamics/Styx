//! Camera hardware traits for Styx: what a platform implements so the camera runtime can run
//! on it, without `std` and without allocation.
//!
//! The bus vocabulary is embedded-hal 1.0 and embedded-hal-async 1.0 (`I2c`, `SpiDevice`,
//! `OutputPin`, `DelayNs`), used directly: generic I²C, SPI, GPIO, delay and clock work belongs
//! to the platform layer (Lemnos on Linux and MCUs, a chip HAL, `styx-kernel` for now). This
//! crate holds only what is specific to cameras:
//!
//! * [`SensorPins`] / [`AsyncSensorPins`]: a sensor's power sequence by role (GPIO lines, the
//!   input clock, supplies) with a `DelayNs`; [`BoardPins`] builds them from embedded-hal
//!   output pins, a [`ClockEnable`] and a delay.
//! * [`DmaMemory`] / [`DmaBuffer`]: buffers devices fill, with cache maintenance and export
//!   handles; [`StaticDma`] carves them out of a linker-placed region.
//! * [`Receiver`]: the CSI-2 or parallel receiver that fills them, with frame-start and
//!   filled-buffer events on separate wakers (`poll_*`) and non-blocking `try_*` twins.
//! * [`LensActuator`] / [`AsyncLensActuator`]: focus lenses that are not a plain register
//!   device.
//! * [`ErrorKind`] / [`HalError`]: what kind of failure an implementation's own error is.
//! * [`Blocking`]: a blocking implementation used through the async traits (always ready).
//!
//! Sensor register access (8/16-bit register addresses, burst writes) is a thin layer over an
//! embedded-hal `I2c` or `SpiDevice` in `styx-sensor` (`I2cRegisters`, `SpiRegisters`).
//!
//! Features: `std` (`MaybeSendSync` is `Send + Sync`, `std::io::Error` as a [`HalError`],
//! [`StdDelay`]); `mock` (a mock platform for host tests: [`mock`]).

#![cfg_attr(not(any(feature = "std", test)), no_std)]

mod dma;
mod error;
mod lens;
#[cfg(feature = "mock")]
pub mod mock;
mod power;
mod receiver;
mod time;

pub use dma::{Access, CacheOps, DmaBuffer, DmaMemory, Region, StaticBuffer, StaticDma};
pub use error::{ErrorKind, HalError};
pub use lens::{AsyncLensActuator, LensActuator, NoLens};
pub use power::{
    AsyncSensorPins, Blocking, BoardPins, ClockEnable, Line, LineKind, NoClock, SensorPins,
};
pub use receiver::{
    BufferSource, Bus, Configured, EmbeddedConfig, FrameDone, MaybeSendSync, Receiver,
    ReceiverCaps, ReceiverConfig, SensorStart, StartOrder, SyncEvent, TimestampPoint,
};
#[cfg(feature = "std")]
pub use time::StdDelay;
pub use time::{Instant, wait, wait_async};

/// The embedded-hal crates this one speaks, re-exported so users name the same versions.
pub use embedded_hal;
pub use embedded_hal_async;

#[cfg(all(test, feature = "mock"))]
mod tests;
