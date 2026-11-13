//! The neutral ISP settings: the one type every ISP maps from.
//!
//! [`IspSettings`] says what an ISP applies to a frame, in hardware-independent terms: full scale
//! 1.0, black level, white balance, digital gain, the colour matrix, the tone curve, lens shading,
//! denoise, sharpening, the luma histogram weights, and the sizes and formats of the outputs. An
//! ISP family's own mapping (Lemnos's layout builders for PiSP, Rockchip, K230 and ISP600, Styx's
//! software and GPU ISPs) turns it into that ISP's values. Nothing here knows a register.
//!
//! Field names and the serialised shape match `styx_controller::IspSettings`, so a settings JSON
//! from the controller reads here unchanged, and this type's JSON compares with it directly. The
//! one addition is `outputs`, left out of the JSON when empty. [`From`] converts the controller's
//! value (its `outputs` start empty).
//!
//! Three operations, each in its own module:
//!
//! * serialisation (serde, always on: the field shape above);
//! * comparison: `==`, and [`IspSettings::changed_fields`] to name what differs;
//! * [`IspSettings::interpolate`] between two tunings (see its docs for the rule per field).
//!
//! `no_std` + `alloc`.

#![no_std]

extern crate alloc;

mod convert;
mod diff;
mod interp;

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};
use styx_aaa::{DenoiseParams, IDENTITY, LensShading, Matrix3, Pwl, SharpenParams, ZoneGrid};
use styx_format::MediaFormat;

/// What an ISP applies to a frame: everything normalised (full scale 1.0) and hardware
/// independent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IspSettings {
    /// The frame whose statistics produced these settings.
    pub from_frame: u64,
    /// Black level to subtract (the smallest of the per-channel levels).
    pub black_level: f64,
    /// Per-channel black levels (red, green on the red row, green on the blue row, blue), when
    /// they are not all equal (see [`Self::black_cfa`]). `None`: every channel is `black_level`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub black_channels: Option<[f64; 4]>,
    /// White balance gains, green 1.
    pub wb: [f64; 3],
    /// Digital gain on top of the white balance (exposure the sensor could not provide, the
    /// difference while a new exposure is still on its way, and deflicker's correction).
    pub digital_gain: f64,
    /// How much brighter than the mean light the flicker made the frame, as deflicker corrects
    /// it (1: no correction). Temporal denoise scales its average by it.
    #[serde(default = "one")]
    pub flicker: f64,
    /// Rolling shutter: deflicker's gains per band of rows (top to bottom), folded into
    /// `lens_shading`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flicker_bands: Option<Vec<f64>>,
    /// Colour correction matrix (row-major, camera RGB to output RGB after white balance).
    pub ccm: Matrix3,
    /// Tone curve on `[0, 1]`; `None` is the sRGB curve.
    pub gamma: Option<Pwl>,
    /// Lens shading gains, if the tuning calibrates them.
    pub lens_shading: Option<LensShading>,
    /// Denoise, green equalisation and defective pixel correction (ISPs with those blocks).
    #[serde(default)]
    pub denoise: DenoiseParams,
    /// Sharpening factors (`None`: the ISP's default).
    pub sharpen: Option<SharpenParams>,
    /// Zone weights of the luma histogram (the metering mode's; `None`: uniform).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram_weights: Option<ZoneGrid<f64>>,
    /// The formats (size, pixel format, colour space and range) of the ISP's outputs, by output
    /// index. Empty: the ISP's own default outputs, as the controller's settings give. Left out
    /// of the serialised form when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<MediaFormat>,
}

fn one() -> f64 {
    1.0
}

impl IspSettings {
    /// Neutral settings: black level only, unity gains, identity matrix, sRGB tone, no outputs.
    pub fn neutral(black_level: f64) -> Self {
        Self {
            from_frame: 0,
            black_level,
            black_channels: None,
            wb: [1.0; 3],
            digital_gain: 1.0,
            flicker: 1.0,
            flicker_bands: None,
            ccm: IDENTITY,
            gamma: None,
            lens_shading: None,
            denoise: DenoiseParams::default(),
            sharpen: None,
            histogram_weights: None,
            outputs: Vec::new(),
        }
    }

    /// The black level of each CFA channel (red, green on the red row, green on the blue row,
    /// blue), normalised.
    pub fn black_cfa(&self) -> [f64; 4] {
        self.black_channels.unwrap_or([self.black_level; 4])
    }

    /// White balance times digital gain, per channel, with an extra gain of 1 / the smallest
    /// white balance gain when that is below 1, so no channel gets less than unity and saturated
    /// pixels stay white instead of turning cyan or magenta.
    pub fn channel_gains(&self) -> [f64; 3] {
        let min = self.wb.iter().copied().fold(1.0, f64::min).max(0.1);
        self.wb.map(|w| w * self.digital_gain / min)
    }
}
