//! The per-frame metadata: what was in force and what the frame measured.

use crate::control::{Control, ControlValue};

/// Metadata of one frame. Fixed size, plain data, no heap: it is the `M` of a
/// `styx_frame::FramePool<M>`, so writing it for every frame allocates nothing.
///
/// `controls` is what each control was set to when the frame was captured, indexed by
/// [`Control::index`]. The measured fields are what the sensor or ISP reports for that frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FrameMetadata {
    /// The control values in force on this frame, indexed by [`Control::index`].
    pub controls: [ControlValue; Control::COUNT],
    /// Sensor timestamp, `CLOCK_MONOTONIC` nanoseconds.
    pub sensor_timestamp_ns: u64,
    /// Exposure time the frame was captured with, in microseconds.
    pub exposure_us: u32,
    /// Analogue gain the frame was captured with, in thousandths (1000 is 1.0x).
    pub analogue_gain_milli: u32,
    /// Digital gain applied, in thousandths (1000 is 1.0x).
    pub digital_gain_milli: u32,
    /// Scene brightness estimate in lux, when the backend has one.
    pub lux: Option<f32>,
    /// Colour temperature of the scene in kelvin, when the backend has one.
    pub colour_temperature_k: Option<u32>,
}

impl FrameMetadata {
    /// The value of `control` in force on this frame.
    pub const fn control(&self, control: Control) -> ControlValue {
        self.controls[control.index()]
    }
}
