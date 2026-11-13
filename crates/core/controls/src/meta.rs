//! The per-frame metadata: what was in force and what the frame measured.

use crate::control::{Control, ControlValue, Rect};

/// Version of the metadata's byte layout (see [`FrameMetadata::to_bytes`]). A reader refuses any
/// other.
pub const WIRE_VERSION: u8 = 1;

/// Bytes of one control's value on the wire: a tag, then 17 bytes of payload.
const VALUE_BYTES: usize = 18;
/// Bytes of [`FrameMetadata::to_bytes`]'s output, and the only length [`FrameMetadata::from_bytes`]
/// takes.
pub const WIRE_BYTES: usize = 32 + Control::COUNT * VALUE_BYTES;

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

impl FrameMetadata {
    /// Writes the metadata into `out` as its fixed little-endian layout, [`WIRE_VERSION`] first,
    /// and returns [`WIRE_BYTES`]. Returns 0, writing nothing, when `out` is shorter than that.
    /// No heap is used.
    ///
    /// Layout: version (1), reserved (1, zero), sensor timestamp (u64), exposure (u32), analogue
    /// gain (u32), digital gain (u32), lux (present byte, then f32 bits), colour temperature
    /// (present byte, then u32), then each control in [`Control::index`] order as a tag (0 auto,
    /// 1 manual, 2 flag, 3 crop) and 17 bytes of payload (a u32, a flag byte, or a crop's present
    /// byte and four u32s), zero where unused.
    pub fn to_bytes(&self, out: &mut [u8]) -> usize {
        let Some(out) = out.get_mut(..WIRE_BYTES) else {
            return 0;
        };
        out.fill(0);
        out[0] = WIRE_VERSION;
        out[2..10].copy_from_slice(&self.sensor_timestamp_ns.to_le_bytes());
        out[10..14].copy_from_slice(&self.exposure_us.to_le_bytes());
        out[14..18].copy_from_slice(&self.analogue_gain_milli.to_le_bytes());
        out[18..22].copy_from_slice(&self.digital_gain_milli.to_le_bytes());
        if let Some(lux) = self.lux {
            out[22] = 1;
            out[23..27].copy_from_slice(&lux.to_bits().to_le_bytes());
        }
        if let Some(k) = self.colour_temperature_k {
            out[27] = 1;
            out[28..32].copy_from_slice(&k.to_le_bytes());
        }
        for (index, value) in self.controls.iter().enumerate() {
            let at = 32 + index * VALUE_BYTES;
            let slot = &mut out[at..at + VALUE_BYTES];
            match *value {
                ControlValue::Auto => slot[0] = 0,
                ControlValue::Manual(v) => {
                    slot[0] = 1;
                    slot[1..5].copy_from_slice(&v.to_le_bytes());
                }
                ControlValue::Flag(on) => {
                    slot[0] = 2;
                    slot[1] = u8::from(on);
                }
                ControlValue::Crop(rect) => {
                    slot[0] = 3;
                    if let Some(r) = rect {
                        slot[1] = 1;
                        slot[2..6].copy_from_slice(&r.x.to_le_bytes());
                        slot[6..10].copy_from_slice(&r.y.to_le_bytes());
                        slot[10..14].copy_from_slice(&r.width.to_le_bytes());
                        slot[14..18].copy_from_slice(&r.height.to_le_bytes());
                    }
                }
            }
        }
        WIRE_BYTES
    }

    /// Reads metadata written by [`FrameMetadata::to_bytes`]. `None` for another length, another
    /// version, a reserved byte that is not zero, or a tag or flag this version does not know.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != WIRE_BYTES || bytes[0] != WIRE_VERSION || bytes[1] != 0 {
            return None;
        }
        let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
        let lux = match bytes[22] {
            0 => None,
            1 => Some(f32::from_bits(u32_at(23)?)),
            _ => return None,
        };
        let colour_temperature_k = match bytes[27] {
            0 => None,
            1 => Some(u32_at(28)?),
            _ => return None,
        };
        let mut controls = [ControlValue::Auto; Control::COUNT];
        for (index, value) in controls.iter_mut().enumerate() {
            let at = 32 + index * VALUE_BYTES;
            let slot = &bytes[at..at + VALUE_BYTES];
            *value = match slot[0] {
                0 => ControlValue::Auto,
                1 => ControlValue::Manual(u32_at(at + 1)?),
                2 => match slot[1] {
                    0 => ControlValue::Flag(false),
                    1 => ControlValue::Flag(true),
                    _ => return None,
                },
                3 => match slot[1] {
                    0 => ControlValue::Crop(None),
                    1 => ControlValue::Crop(Some(Rect {
                        x: u32_at(at + 2)?,
                        y: u32_at(at + 6)?,
                        width: u32_at(at + 10)?,
                        height: u32_at(at + 14)?,
                    })),
                    _ => return None,
                },
                _ => return None,
            };
        }
        Some(Self {
            controls,
            sensor_timestamp_ns: u64_at(2)?,
            exposure_us: u32_at(10)?,
            analogue_gain_milli: u32_at(14)?,
            digital_gain_milli: u32_at(18)?,
            lux,
            colour_temperature_k,
        })
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn sample() -> FrameMetadata {
        let mut controls = [ControlValue::Auto; Control::COUNT];
        controls[Control::Exposure.index()] = ControlValue::Manual(5_000);
        controls[Control::Gain.index()] = ControlValue::Manual(2_500);
        controls[Control::HFlip.index()] = ControlValue::Flag(true);
        controls[Control::Crop.index()] = ControlValue::Crop(Some(Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }));
        FrameMetadata {
            controls,
            sensor_timestamp_ns: 9_876_543_210,
            exposure_us: 5_000,
            analogue_gain_milli: 2_500,
            digital_gain_milli: 1_000,
            lux: Some(312.5),
            colour_temperature_k: Some(5_600),
        }
    }

    #[test]
    fn metadata_round_trips_every_field() {
        let meta = sample();
        let mut out = [0_u8; WIRE_BYTES];
        assert_eq!(meta.to_bytes(&mut out), WIRE_BYTES);
        assert_eq!(FrameMetadata::from_bytes(&out), Some(meta));
        let bare = FrameMetadata::default();
        let mut out = [0_u8; WIRE_BYTES];
        bare.to_bytes(&mut out);
        assert_eq!(FrameMetadata::from_bytes(&out), Some(bare));
    }

    #[test]
    fn a_short_buffer_a_wrong_version_and_a_wrong_length_are_refused() {
        let meta = sample();
        let mut small = [0_u8; WIRE_BYTES - 1];
        assert_eq!(meta.to_bytes(&mut small), 0);
        let mut out = [0_u8; WIRE_BYTES];
        meta.to_bytes(&mut out);
        let mut other_version = out;
        other_version[0] = WIRE_VERSION + 1;
        assert_eq!(FrameMetadata::from_bytes(&other_version), None);
        assert_eq!(FrameMetadata::from_bytes(&out[..WIRE_BYTES - 1]), None);
        let mut longer = out.to_vec();
        longer.push(0);
        assert_eq!(FrameMetadata::from_bytes(&longer), None);
        let mut bad_tag = out;
        bad_tag[32] = 9;
        assert_eq!(FrameMetadata::from_bytes(&bad_tag), None);
    }
}
