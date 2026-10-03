//! Per-frame inputs besides statistics: what the sensor did and what the application asked for.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Names of the metering modes found in tunings.
pub mod metering {
    /// Centre weighted.
    pub const CENTRE_WEIGHTED: &str = "centre-weighted";
    /// Spot.
    pub const SPOT: &str = "spot";
    /// Average over the frame (Raspberry Pi tunings call it "matrix").
    pub const AVERAGE: &str = "matrix";
}

/// Flicker avoidance.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flicker {
    /// No avoidance.
    #[default]
    Off,
    /// 50 Hz mains: light flickers at 100 Hz, exposures become multiples of 10 ms.
    Mains50,
    /// 60 Hz mains: light flickers at 120 Hz, exposures become multiples of 8.33 ms.
    Mains60,
    /// A given flicker period.
    Period(Duration),
    /// Detect 50 or 60 Hz mains flicker from the frames' brightness and avoid it as if set;
    /// nothing until detected (the detected period: `AeStatus::flicker_detected`).
    Auto,
}

impl Flicker {
    /// The flicker period to avoid, if set (`None` for [`Flicker::Auto`]: it depends on the
    /// detection).
    pub fn period(self) -> Option<Duration> {
        match self {
            Flicker::Off | Flicker::Auto => None,
            Flicker::Mains50 => Some(Duration::from_nanos(10_000_000)),
            Flicker::Mains60 => Some(Duration::from_nanos(8_333_333)),
            Flicker::Period(p) if p.is_zero() => None,
            Flicker::Period(p) => Some(p),
        }
    }
}

/// Taking the flicker out of the frames: each frame's ISP digital gain divided by the
/// brightness AE's flicker model predicts for it (exposures shorter than a flicker period,
/// where avoidance cannot help; see `algos::agc::deflicker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deflicker {
    /// Never.
    Off,
    /// Whenever the frames flicker (fitting 50 and 60 Hz mains as [`Flicker::Auto`] does
    /// when flicker avoidance is off).
    On,
    /// When flicker avoidance is on (the default).
    #[default]
    Auto,
}

impl Deflicker {
    /// Whether it is on with this flicker avoidance.
    pub fn enabled(self, flicker: Flicker) -> bool {
        match self {
            Deflicker::Off => false,
            Deflicker::On => true,
            Deflicker::Auto => flicker != Flicker::Off,
        }
    }
}

/// What the application asks for. Recorded per frame, so replays reproduce control changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Controls {
    /// Automatic exposure and gain.
    pub ae_enable: bool,
    /// Fixed exposure time (AE then only moves gain).
    pub exposure: Option<Duration>,
    /// Fixed analogue gain (AE then only moves exposure time).
    pub analogue_gain: Option<f64>,
    /// Exposure compensation in stops.
    pub ev: f64,
    /// Metering mode by tuning name (see [`metering`]); `None` is the tuning's default.
    pub metering_mode: Option<String>,
    /// Exposure mode (shutter/gain profile) by tuning name.
    pub exposure_mode: Option<String>,
    /// Constraint mode by tuning name.
    pub constraint_mode: Option<String>,
    /// Flicker avoidance.
    pub flicker: Flicker,
    /// Taking the flicker out of the frames.
    pub deflicker: Deflicker,
    /// Frame duration limits (min, max), within the camera configuration's.
    pub frame_duration_limits: Option<(Duration, Duration)>,
    /// Automatic white balance.
    pub awb_enable: bool,
    /// AWB mode by tuning name (e.g. "auto", "daylight"); `None` is the tuning's default.
    pub awb_mode: Option<String>,
    /// Manual red and blue gains (used when AWB is off).
    pub colour_gains: Option<(f64, f64)>,
    /// Manual colour temperature in kelvin (used when AWB is off and no gains are given).
    pub colour_temperature: Option<f64>,
    /// Colour saturation factor.
    pub saturation: f64,
    /// Brightness offset, in normalised output units.
    pub brightness: f64,
    /// Contrast factor about mid-grey.
    pub contrast: f64,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            ae_enable: true,
            exposure: None,
            analogue_gain: None,
            ev: 0.0,
            metering_mode: None,
            exposure_mode: None,
            constraint_mode: None,
            flicker: Flicker::Off,
            deflicker: Deflicker::Auto,
            frame_duration_limits: None,
            awb_enable: true,
            awb_mode: None,
            colour_gains: None,
            colour_temperature: None,
            saturation: 1.0,
            brightness: 0.0,
            contrast: 1.0,
        }
    }
}

/// What produced a frame, as reported by the sensor path (e.g. `ControlScheduler::applied` in
/// `styx-sensor`, converted from register codes to physical units).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameMetadata {
    /// Frame sequence number.
    pub frame: u64,
    /// Exposure time that produced the frame.
    pub exposure: Duration,
    /// Analogue gain that produced the frame.
    pub analogue_gain: f64,
    /// Sensor digital gain that produced the frame (1 when unused).
    #[serde(default = "one")]
    pub digital_gain: f64,
    /// Frame duration.
    pub frame_duration: Duration,
    /// Scene illuminance from another source (e.g. a light sensor), overriding the estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lux: Option<f64>,
    /// Application controls in effect for this frame's processing.
    #[serde(default)]
    pub controls: Controls,
}

fn one() -> f64 {
    1.0
}

impl FrameMetadata {
    /// Metadata with default controls.
    pub fn new(
        frame: u64,
        exposure: Duration,
        analogue_gain: f64,
        frame_duration: Duration,
    ) -> Self {
        Self {
            frame,
            exposure,
            analogue_gain,
            digital_gain: 1.0,
            frame_duration,
            lux: None,
            controls: Controls::default(),
        }
    }
}
