//! The output: sensor settings and ISP parameters for coming frames.
//!
//! [`Params`] is also how algorithms talk to each other within one frame: the pipeline runs
//! them in order over the same `Params`, so e.g. CCM reads the colour temperature AWB wrote.
//! Values persist from frame to frame until an algorithm changes them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::pwl::Pwl;
use crate::stats::ZoneGrid;

/// A 3×3 row-major colour matrix.
pub type Matrix3 = [f64; 9];

/// The identity matrix.
pub const IDENTITY: Matrix3 = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// Multiply two row-major 3×3 matrices.
pub fn mat_mul(a: &Matrix3, b: &Matrix3) -> Matrix3 {
    let mut out = [0.0; 9];
    for r in 0..3 {
        for c in 0..3 {
            out[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum();
        }
    }
    out
}

/// Exposure, gain and frame duration for the sensor, and the frame they apply from.
///
/// AE writes one every frame; a request equal to the previous frame's (same `frame` and
/// values) means nothing new, e.g. while AE waits for a change to land.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SensorRequest {
    /// First frame the values should apply to. Hand this to the control scheduler
    /// (`ControlScheduler::request(frame, ..)`), which issues each control `delay` frames
    /// earlier so all of them land together on this frame.
    pub frame: u64,
    /// Exposure time.
    pub exposure: Duration,
    /// Analogue gain.
    pub analogue_gain: f64,
    /// Frame duration.
    pub frame_duration: Duration,
}

/// AE state, for applications and for other algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AeStatus {
    /// Exposure has settled (stable for several frames).
    pub locked: bool,
    /// Total exposure (seconds × analogue gain × digital gain) being aimed for.
    pub target_exposure: f64,
    /// Total exposure requested this frame after damping.
    pub total_exposure: f64,
    /// Luma target in effect.
    pub target_y: f64,
    /// Measured (metered) luma of this frame.
    pub measured_y: f64,
    /// The exposure was cut quickly to leave saturation.
    pub desaturating: bool,
    /// The flicker period exposures of at least that long are whole multiples of (the mains
    /// period when the lamp flickers at the mains frequency, else half of it), while flicker
    /// avoidance is on and a mains frequency is set or detected.
    #[serde(default)]
    pub flicker_period: Option<Duration>,
    /// What [`crate::Flicker::Auto`] detected, as the light flicker period of full-wave lamps
    /// on that mains (10 ms for 50 Hz mains, 8.33 ms for 60 Hz).
    #[serde(default)]
    pub flicker_detected: Option<Duration>,
    /// How much brighter (relative) than the mean light the flicker made this frame, as AE
    /// estimated and metered it (0 without a flicker fit).
    #[serde(default)]
    pub flicker_modulation: f64,
    /// The scene wants more (or less) exposure than AE can give and AE is at its limit (it
    /// then locks there: `locked` means settled, not on target).
    #[serde(default)]
    pub at_limit: bool,
}

/// AWB state.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AwbStatus {
    /// AWB is automatic (not manual gains or temperature).
    pub auto: bool,
    /// The AWB mode in use.
    pub mode: String,
    /// The unfiltered estimate: (colour temperature, red gain, blue gain).
    pub estimate: (f64, f64, f64),
    /// Filtered gains are within 1% of the estimate.
    pub converged: bool,
}

/// Per-channel black levels, normalised to full scale 1.0.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct BlackLevels {
    /// Red.
    pub r: f64,
    /// Green.
    pub g: f64,
    /// Blue.
    pub b: f64,
}

/// Lens-shading gain tables, row-major over a grid covering the output image.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LensShading {
    /// Cells across.
    pub width: u32,
    /// Cells down.
    pub height: u32,
    /// Red gains.
    pub r: Vec<f64>,
    /// Green gains.
    pub g: Vec<f64>,
    /// Blue gains.
    pub b: Vec<f64>,
}

/// Spatial denoise for the ISP (16-bit pixel scale, see [`crate::algos::denoise`]).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SdnParams {
    /// Noise constant.
    pub noise_constant: f64,
    /// Noise slope.
    pub noise_slope: f64,
    /// Second stage noise constant.
    pub noise_constant2: f64,
    /// Second stage noise slope.
    pub noise_slope2: f64,
    /// Strength (1 − the proportion of the original let through).
    pub strength: f64,
}

/// Colour denoise for the ISP.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CdnParams {
    /// Threshold (16-bit scale).
    pub threshold: f64,
    /// IIR strength.
    pub strength: f64,
}

/// Temporal denoise for the ISP.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TdnParams {
    /// Noise constant.
    pub noise_constant: f64,
    /// Noise slope.
    pub noise_slope: f64,
    /// Threshold (a fraction).
    pub threshold: f64,
}

/// Green equalisation for the ISP.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct GeqParams {
    /// Offset (16-bit scale).
    pub offset: f64,
    /// Slope.
    pub slope: f64,
}

/// Noise-dependent ISP settings; `None` blocks are off.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DenoiseParams {
    /// Noise profile of this frame: constant (16-bit scale).
    pub noise_constant: f64,
    /// Noise profile slope.
    pub noise_slope: f64,
    /// Spatial denoise.
    pub sdn: Option<SdnParams>,
    /// Colour denoise.
    pub cdn: Option<CdnParams>,
    /// Temporal denoise.
    pub tdn: Option<TdnParams>,
    /// Green equalisation.
    pub geq: Option<GeqParams>,
    /// Defective pixel correction: 0 off, 1 normal, 2 strong.
    pub dpc: u8,
}

/// Sharpening as factors on the ISP's default sharpening.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SharpenParams {
    /// Threshold factor.
    pub threshold: f64,
    /// Strength factor.
    pub strength: f64,
    /// Limit factor.
    pub limit: f64,
}

/// Everything the algorithms produce for a frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Params {
    /// Sensor settings to request, if AE produced any.
    pub sensor: Option<SensorRequest>,
    /// Digital gain for the ISP (on top of the colour gains).
    pub digital_gain: f64,
    /// AE state.
    pub ae: AeStatus,
    /// Scene illuminance estimate (lux).
    pub lux: f64,
    /// White balance gains (r, g, b).
    pub colour_gains: [f64; 3],
    /// Estimated colour temperature (kelvin).
    pub colour_temperature: f64,
    /// AWB state.
    pub awb: AwbStatus,
    /// Colour correction matrix (camera RGB to output RGB, after white balance).
    pub ccm: Matrix3,
    /// Tone curve on normalised values, `[0, 1] -> [0, 1]`.
    pub gamma: Option<Pwl>,
    /// Black levels to subtract.
    pub black_level: BlackLevels,
    /// Lens-shading tables.
    pub lens_shading: Option<LensShading>,
    /// Denoise, green equalisation and defective pixel correction.
    #[serde(default)]
    pub denoise: DenoiseParams,
    /// Sharpening (`None`: the ISP's default).
    #[serde(default)]
    pub sharpen: Option<SharpenParams>,
    /// Zone weights of the metering mode in use, on the tuning's grid (15×15 for Raspberry Pi
    /// tunings), for ISPs that weight their luma histogram by zone (the PiSP front end, as the
    /// Raspberry Pi IPA programs it). Written by AGC; `None`: unweighted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram_weights: Option<ZoneGrid<f64>>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            sensor: None,
            digital_gain: 1.0,
            ae: AeStatus::default(),
            lux: 400.0,
            colour_gains: [1.0; 3],
            colour_temperature: 4500.0,
            awb: AwbStatus::default(),
            ccm: IDENTITY,
            gamma: None,
            black_level: BlackLevels::default(),
            lens_shading: None,
            denoise: DenoiseParams::default(),
            sharpen: None,
            histogram_weights: None,
        }
    }
}
