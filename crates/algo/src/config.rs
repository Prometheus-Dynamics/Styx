//! Camera configuration given to [`crate::Algorithm::prepare`]: the sensor mode's limits and
//! control timing.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AlgoError, Result};

/// Frame delays of the sensor controls, as in `styx-sensor`'s `Delays`, plus the issue latency.
///
/// A value written during frame `S` first affects frame `S + delay`. Statistics for frame `F`
/// are processed during frame `F + 1`, so the earliest frame start at which new values can be
/// written is `F + issue_latency` (2 by default) and the earliest frame on which a full set of
/// exposure, gain and frame duration lands together is
/// `F + issue_latency + max(delays)` ([`ControlDelays::earliest_landing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlDelays {
    /// Exposure delay in frames.
    pub exposure: u32,
    /// Analogue gain delay in frames.
    pub analogue_gain: u32,
    /// Frame length (duration) delay in frames.
    pub frame_duration: u32,
    /// Frames from the frame the statistics came from to the frame start where writes go out.
    pub issue_latency: u32,
}

impl Default for ControlDelays {
    /// libcamera's defaults for sensors without a helper (exposure 2, gain 1, vblank 2).
    fn default() -> Self {
        Self {
            exposure: 2,
            analogue_gain: 1,
            frame_duration: 2,
            issue_latency: 2,
        }
    }
}

impl ControlDelays {
    /// The largest control delay.
    pub fn max(&self) -> u32 {
        self.exposure
            .max(self.analogue_gain)
            .max(self.frame_duration)
    }

    /// First frame that values computed from frame `frame`'s statistics can all apply from.
    pub fn earliest_landing(&self, frame: u64) -> u64 {
        frame + u64::from(self.issue_latency) + u64::from(self.max())
    }
}

/// The part of the pixel array the output covers, as fractions of the full array (for lens
/// shading tables calibrated over the full array).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

impl Default for Crop {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        }
    }
}

/// The sensor mode as algorithms see it. Fill it from `styx-sensor`'s `Timing` (exposure and
/// frame duration limits), the gain model (analogue gain limits) and `Delays`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    /// Shortest and longest exposure the sensor mode can do (the longest at the longest frame).
    pub exposure_limits: (Duration, Duration),
    /// Minimum difference between frame duration and exposure time.
    pub exposure_margin: Duration,
    /// Shortest and longest frame duration.
    pub frame_duration_limits: (Duration, Duration),
    /// Smallest and largest analogue gain.
    pub analogue_gain_limits: (f64, f64),
    /// Control delays.
    pub delays: ControlDelays,
    /// Sensitivity of this mode relative to the tuned one (e.g. 2 for 2×2 summing binning).
    pub sensitivity: f64,
    /// Black level from the sensor description, normalised, used when the tuning has none.
    pub black_level: Option<f64>,
    /// Frames at the start of a stream whose levels are not reliable yet (the sensor's black
    /// level calibration settling, an exposure cut short by the stream's start): AE and AWB
    /// leave them out.
    pub unsettled_frames: u32,
    /// Output crop within the pixel array.
    pub crop: Crop,
    /// Horizontal flip.
    pub hflip: bool,
    /// Vertical flip.
    pub vflip: bool,
    /// The ISP runs temporal denoise (keeps a long-term average of frames), so spatial and
    /// colour denoise can back off to their with-TDN strengths.
    pub temporal_denoise: bool,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            exposure_limits: (Duration::from_micros(10), Duration::from_millis(100)),
            exposure_margin: Duration::ZERO,
            frame_duration_limits: (Duration::from_nanos(8_333_333), Duration::from_millis(100)),
            analogue_gain_limits: (1.0, 16.0),
            delays: ControlDelays::default(),
            sensitivity: 1.0,
            black_level: None,
            unsettled_frames: 0,
            crop: Crop::default(),
            hflip: false,
            vflip: false,
            temporal_denoise: false,
        }
    }
}

impl CameraConfig {
    /// Check the configuration is usable.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: &str| Err(AlgoError::Config(m.into()));
        if self.exposure_limits.0.is_zero() || self.exposure_limits.0 > self.exposure_limits.1 {
            return bad("exposure limits must be non-zero and ordered");
        }
        if self.frame_duration_limits.0.is_zero()
            || self.frame_duration_limits.0 > self.frame_duration_limits.1
        {
            return bad("frame duration limits must be non-zero and ordered");
        }
        let (g0, g1) = self.analogue_gain_limits;
        if !(g0 > 0.0 && g0 <= g1 && g1.is_finite()) {
            return bad("analogue gain limits must be positive and ordered");
        }
        if !(self.sensitivity > 0.0 && self.sensitivity.is_finite()) {
            return bad("sensitivity must be positive");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earliest_landing_uses_largest_delay() {
        let d = ControlDelays {
            exposure: 2,
            analogue_gain: 1,
            frame_duration: 3,
            issue_latency: 2,
        };
        assert_eq!(d.earliest_landing(10), 15);
    }

    #[test]
    fn default_config_is_valid() {
        CameraConfig::default().validate().unwrap();
        let bad = CameraConfig {
            analogue_gain_limits: (2.0, 1.0),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
    }
}
