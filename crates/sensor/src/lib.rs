//! Camera sensors as data: sensor descriptions (TOML), the timing model (frame rates from pixel
//! rate and blanking), exposure and gain models, frame-accurate control scheduling, and a
//! userspace sensor driver that runs a description over a register bus.
//!
//! This crate does not touch the kernel. Register access goes through [`RegisterBus`] (Lemnos's
//! register maps, [`I2cRegisters`] / [`SpiRegisters`], on any embedded-hal bus) and
//! power/reset/clock lines through [`SensorPins`]; the buses themselves are Lemnos's
//! (`lemnos-linux` on Linux, a chip HAL on microcontrollers). See `docs/native-stack/README.md` for where this fits.
//!
//! # Sensor descriptions
//!
//! One TOML file per sensor. Unknown fields are rejected and every value is range-checked
//! ([`SensorDescription::from_toml_str`] reports all problems with their paths). Numbers may be
//! hex (`0x3500`) and use `_` separators. A register write is `[address, value]` or
//! `[address, value, bytes]`; multi-byte registers are consecutive addresses, most significant
//! byte first.
//!
//! ```
//! use styx_sensor::SensorDescription;
//!
//! let desc = SensorDescription::from_toml_str(r#"
//! [sensor]
//! name = "example"
//! i2c_address = 0x10                    # 7-bit address
//! address_bits = 16                     # register address width: 8 or 16
//! chip_id = { address = 0x0000, bytes = 2, values = [0x0219] }
//! clocks = { xclk = 24_000_000 }        # clock roles and rates
//! tuning = "example.json"
//!
//! [pixel_array]
//! size = [3296, 2480]                   # including dummy / optical black pixels
//! active = { left = 8, top = 8, width = 3280, height = 2464 }
//! color_filter = "RGGB"                 # BGGR, GBRG, GRBG, RGGB or mono
//! black_level = { value = 64, bits = 10 }
//!
//! [sequences]
//! power_up = [
//!     { supply = "vana", on = true, optional = true },
//!     { clock = "xclk", on = true },
//!     { gpio = "powerdown", value = 0, optional = true },  # logical value, by role
//!     { delay_ms = 5 },
//! ]
//! power_down = [{ clock = "xclk", on = false }]
//! init = [[0x30eb, 0x05], [0x30eb, 0x0c]]
//! stream_on = [[0x0100, 0x01]]
//! stream_off = [[0x0100, 0x00]]
//!
//! [formats.raw10]                       # a bit depth: media bus code and pixel rate
//! code = "SRGGB10_1X10"
//! pixel_rate = 182_400_000
//! link_frequency = 456_000_000
//! registers = [[0x018c, 0x0a0a, 2]]
//!
//! [[modes]]
//! name = "1640x1232"
//! size = [1640, 1232]
//! crop = { left = 8, top = 8, width = 3280, height = 2464 }  # in the pixel array
//! binning = [2, 2]
//! hblank = { min = 1808, max = 1808, default = 1808 }         # pixels
//! vblank = { min = 4, max = 64303, default = 1450 }           # lines
//! registers = [[0x0174, 0x01], [0x0175, 0x01]]
//!
//! [controls]
//! frame_length = { address = 0x0160, bytes = 2 }              # VTS, lines
//! line_length = { register = { address = 0x0162, bytes = 2 } }
//! delays = { exposure = 2, analog_gain = 1, frame_length = 2 }
//! group_hold = { start = [[0x0104, 0x01]], end = [[0x0104, 0x00]] }
//! hflip = { address = 0x0172, mask = 0x01, changes_bayer_order = true }
//!
//! [controls.exposure]                   # lines
//! register = { address = 0x015a, bytes = 2 }
//! min = 4
//! margin = 4                            # exposure <= frame_length - margin
//! default = 1000
//!
//! [controls.analog_gain]
//! register = { address = 0x0157 }
//! min_code = 0
//! max_code = 232
//! default_code = 0
//! model = { reciprocal = { numerator = 256, base = 256 } }  # or linear / table
//!
//! [controls.test_pattern]
//! register = { address = 0x0600, bytes = 2 }
//! patterns = { off = 0, colour_bars = 2 }
//! "#, "example.toml").unwrap();
//!
//! // fps = pixel_rate / (line_length x frame_length)
//! let timing = desc.timing("1640x1232", "raw10").unwrap();
//! let (min_fps, max_fps) = timing.fps_range();
//! assert!(max_fps > 40.0 && min_fps < 1.0);
//! let thirty = timing.frame_length_for_fps(30.0);
//! assert!((thirty.fps - 30.0).abs() < 0.01);
//! ```
//!
//! Field reference (all sections reject unknown keys):
//!
//! * `[sensor]`: `name`, `vendor`, `backend` (`registers` or `kernel`), `i2c_address`,
//!   `address_bits`, `chip_id`, `clocks`, `tuning`.
//! * `[pixel_array]`: `size`, `active`, `color_filter`, `black_level`.
//! * `[sequences]`: `power_up`, `power_down` (may use `supply`, `clock`, `gpio`, `delay_us`,
//!   `delay_ms` steps; add `optional = true` to skip roles the board lacks), `init`,
//!   `stream_on`, `stream_off` (register writes and delays only).
//! * `[formats.<name>]`: `code`, `bit_depth` (only for codes this crate does not know),
//!   `pixel_rate`, `link_frequency`, `registers`, `embedded_data` (default `true`; `false`
//!   when the `[embedded_data]` layout does not hold in this bit depth).
//! * `[[modes]]`: `name`, `size`, `crop`, `formats` (default all), `binning`, `skipping`,
//!   `hblank`, `vblank`, `pixel_rate` (override), `registers`.
//! * `[controls]`: `frame_length`, `line_length` (`register`, `pixels_per_unit`), `exposure`
//!   (`register`, `fraction_bits`, `min`, `margin`, `step`, `default`), `analog_gain` and
//!   `digital_gain` (`register`, `min_code`, `max_code`, `default_code`, `model`), `delays`,
//!   `group_hold` (`start`, `end`, `launch`), `hflip`/`vflip` (`address`, `mask`, `default`,
//!   `changes_bayer_order`), `test_pattern` (`register`, `patterns`).
//! * `[bus]` (only where no device tree describes the bus, e.g. microcontrollers): `parallel =
//!   { width, pclk_rising, hsync_active_high, vsync_active_high, embedded_sync }` or `csi2 =
//!   { lanes, continuous_clock, virtual_channel }`.
//! * `[embedded_data]`: `lines`, `packing` (`none` or `raw10`), `entries = [{ address, offset }]`,
//!   `controls = [{ control, offset, bytes, shift }]`.
//!
//! Register fields are `{ address, bytes = 1, shift = 0, bits = rest, read_modify_write =
//! false }`.
//!
//! # Pieces
//!
//! * [`Timing`]: frame rate ranges, frame length for a target rate or rate range, exposure
//!   limits and conversions between durations and lines.
//! * [`Gain::code_for_gain`] and [`split_gain`]: gain ratios to codes, reporting the gain
//!   actually obtained.
//! * [`ControlScheduler`]: which writes to issue at each frame start so values land on the
//!   requested frame, and which values produced each frame.
//! * [`SensorDriver`]: power, chip id, init, modes, streaming and scheduled controls over a
//!   [`RegisterBus`]; [`MockBus`] and [`MockPins`] record operations for tests.
//! * [`lens`]: focus lenses (VCMs) as data: which chip (driven by `lemnos-drivers-vcm`), the
//!   move time model, the frame-exact [`LensSchedule`], and the IMX708's phase detection data.
//! * [`SensorDescription::from_subdev_with`]: descriptions of sensors with kernel drivers,
//!   built from a [`SubdevReport`] and, when there is one, a [`KernelSensorData`] file (gain
//!   model, delays, black level, embedded data layout; `sensors/kernel/*.toml` ship built in).
//!   [`SensorDriver`] then drives them through V4L2 controls ([`RegisterBus::set_controls`]).
//!
//! # Compiled descriptions
//!
//! With feature `postcard`, [`SensorDescription::from_postcard`] reads a description in a
//! compact binary form, so a target without `std` carries no TOML parser; feature `build`'s
//! [`build`](crate::build) module validates TOML files in a build script and writes that form
//! for [`include_description!`] (a broken description fails the firmware build).
//!
//! # `no_std`
//!
//! Without the default `std` feature the crate is `no_std` + `alloc`: descriptions from
//! strings, timing, gain models, the control scheduler, embedded data, lenses and the driver
//! over a [`RegisterBus`]. `from_file`, [`NoPins`] and the sleeping default of
//! [`SensorPins::delay`] need `std`. Bus and pin operations fail with a [`BusError`] (with
//! `std`, `std::io::Error` converts to and from it; docs/portability.md).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// The `Arc` drivers share their description through: `alloc::sync`'s, or on targets without
/// compare-and-swap (Cortex-M0+, RISC-V without `a`) `portable-atomic-util`'s, the same type as
/// `styx_core::sync::Arc` on every target.
#[cfg(target_has_atomic = "ptr")]
pub use alloc::sync::Arc;
#[cfg(not(target_has_atomic = "ptr"))]
pub use portable_atomic_util::Arc;

#[cfg(feature = "build")]
pub mod build;
mod bus;
mod bus_error;
#[cfg(feature = "postcard")]
mod compiled;
mod desc;
mod driver;
mod embedded;
mod error;
mod fallback;
mod fixed;
mod frame_map;
mod gain;
mod kernel_data;
pub mod lens;
mod math;
mod mbus;
mod registers;
pub mod schedule;
mod timing;

pub use bus::{AsyncRegisterBus, BusOp, BusResult, MockBus, MockPins, NoPins, PinOp, RegisterBus};
pub use bus_error::{BusError, BusErrorKind};
pub use desc::{
    Backend, BlackLevel, Blanking, BusSection, ChipId, Controls, Csi2Bus, Delays, EmbeddedControl,
    EmbeddedControlKind, EmbeddedData, EmbeddedEntry, EmbeddedFormat, EmbeddedPacking,
    EmbeddedRegister, Exposure, Field, Flip, Format, Gain, GainModel, GroupHold, Identity,
    LineLength, Mode, ParallelBus, PixelArray, Rect, RegWrite, SensorDescription, Sequences, Size,
    Step, TestPattern, show_write,
};
pub use driver::{
    ActiveMode, AppliedControls, AsyncSensorDriver, ControlRequest, DriverState, SensorDriver,
};
pub use embedded::unpack_raw10_bytes as embedded_unpack_raw10;
pub use error::{Issue, Issues, Result, SensorError};
pub use fallback::{
    ControlRange, KernelControl, KernelControls, SubdevFormat, SubdevReport, kernel_controls,
};
pub use fixed::FixedVec;
pub use gain::{GainCode, GainSplit, Rounding, split_gain};
pub use kernel_data::{BUILTIN_KERNEL_DATA, KernelSensorData};
/// Lemnos's hardware vocabulary (register maps, regulators, clocks, error kinds) and its VCM
/// lens drivers, re-exported so users name the same versions.
pub use lemnos_drivers_vcm;
pub use lemnos_hal;
pub use lens::{LensDescription, LensFrame, LensMotion, LensSchedule, VcmChip, VcmFormat, VcmI2c};
pub use mbus::{ColorFilter, MbusCode};
pub use registers::{
    AddressWidth, Endian, I2cRegisters, MAX_BURST, Registers, SpiRegisters, read_bytewise,
};
pub use schedule::{
    Applied, Control, ControlScheduler, ControlSet, ExposureLimit, IssueBatch, Landing, Landings,
    Mismatch, Mismatches,
};
/// The camera hardware traits (`styx-hal`): power sequencing ([`SensorPins`],
/// [`AsyncSensorPins`]), the [`Blocking`] adapter, and the embedded-hal crates it speaks.
pub use styx_hal;
pub use styx_hal::{AsyncSensorPins, Blocking, SensorPins};
pub use timing::{ExposureLimits, ExposureSpec, ExposureValue, FrameLength, Timing};

/// Sensor descriptions that ship with this crate (`sensors/*.toml`): `(sensor name, TOML)`.
/// Programs embed them as the last entry of their search path, so a file of the same name
/// installed on the system (or on `STYX_SENSOR_PATH`) overrides them.
///
/// * `ov9782`: register values from the HeliOS `ov9782.c` driver, cleared for use in Styx by
///   its author (see the file's header).
pub const BUILTIN_DESCRIPTIONS: &[(&str, &str)] =
    &[("ov9782", include_str!("../sensors/ov9782.toml"))];
