//! Controls and per-frame metadata.
//!
//! - [`Control`]: the one control set. Every backend (native, libcamera, V4L2) maps its own
//!   names and units onto these: exposure, gain, white balance, focus, frame rate, horizontal and
//!   vertical flip, crop, and the image and lens controls every webcam has (brightness, contrast,
//!   saturation, hue, sharpness, gamma, backlight compensation, power line frequency, zoom, pan
//!   and tilt).
//! - [`Spec`]: each control's range, default and whether it has an automatic mode. The range and
//!   default are data here, so a test can check every backend against them.
//! - [`ControlValue`]: a value of one control. Plain data, `Copy`, no heap.
//! - [`Update`]: a value and the frame id ([`styx_frame::FrameEntry::id`]) it first applies to.
//! - [`DeviceControl`]: a control a device offers that has no named [`Control`]. Backends list
//!   every device control; the ones that are not mapped are read and written by id.
//! - [`FrameMetadata`]: the controls in force on a frame, its sensor timestamp, exposure, gains,
//!   lux and colour temperature. Fixed size and `Copy`, so it is the `M` of a
//!   `styx_frame::FramePool<M>`: writing it per frame allocates nothing.
//!
//! Units: exposure in microseconds, gains in thousandths (1000 is 1.0x), frame rate in
//! millihertz, colour temperature in kelvin, crop in pixels of the sensor's pixel array, and
//! timestamps in `CLOCK_MONOTONIC` nanoseconds.
//!
//! `no_std`, no allocation. The `std` feature adds the error trait; `serde` adds serialisation.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod control;
mod device;
mod meta;

pub use control::{
    Control, ControlValue, FrameId, PowerLineFrequency, Range, Rect, Spec, Update, ValueError,
};
pub use device::{DEVICE_NAME_CAPACITY, DeviceControl, DeviceKind, DeviceName};
pub use meta::FrameMetadata;
pub use meta::{WIRE_BYTES, WIRE_VERSION};
