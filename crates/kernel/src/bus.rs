//! Buses a userspace sensor driver uses: I²C (i2c-dev), GPIO (gpio character devices, uAPI v2)
//! and the Styx sensor bridge protocol.
//!
//! - [`i2c::I2cDevice`]: one target address, claimed with `I2C_SLAVE`, combined `I2C_RDWR`
//!   register reads and writes (8/16-bit addresses, 8/16/32-bit values, bursts).
//! - [`gpio::GpioChip`] / [`gpio::GpioLines`]: request output lines by offset or name, set them,
//!   release them on drop.
//! - [`eh`]: embedded-hal 1.0 over them ([`I2cDevice`] is an `I2c`, [`GpioPin`] an
//!   `OutputPin`), so generic bus code runs on Linux unchanged.
//! - [`bridge::SensorBridge`]: the userspace side of `kernel-modules/styx-sensor-bridge`
//!   (`PROTOCOL.md`): pad format, timing controls, power, stream start/stop handshake.

pub mod bridge;
mod bridge_sys;
pub mod eh;
pub mod gpio;
pub mod i2c;
mod ioctl;

pub use bridge::{
    BridgeLocation, PadFormat, SensorBridge, StreamAction, StreamRequest, StreamState, Timing,
    find_bridges,
};
pub use eh::{GpioPin, IoError};
pub use gpio::{Drive, GpioChip, GpioLines, LineId, LineInfo, OutputLine};
pub use i2c::{AddrWidth, I2cDevice, Message, ValueWidth};
