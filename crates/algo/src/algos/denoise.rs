//! Noise profile, denoise (spatial, colour, temporal), green equalisation, defective pixel
//! correction and sharpening strengths for an ISP that has those blocks (the PiSP back end).
//!
//! Ported from Raspberry Pi's `noise.cpp`, `denoise.cpp`, `geq.cpp`, `dpc.cpp` and
//! `sharpen.cpp` (BSD-2-Clause, Copyright (C) 2019-2022 Raspberry Pi Ltd). The noise profile
//! scales with the square root of the analogue gain; spatial and colour denoise start from
//! their "no temporal denoise" strengths and back off towards their tuned values while the
//! temporal denoise builds up its average (every run of the algorithm, as the original does
//! every frame); green equalisation scales with the gain (and lux if tuned).
//!
//! Units are the Raspberry Pi tuning's: noise constants, thresholds and the green
//! equalisation offset on the 16-bit pixel scale, slopes and strengths as plain factors.

use alloc::format;

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
use crate::params::{
    CdnParams, DenoiseParams, GeqParams, Params, SdnParams, SharpenParams, TdnParams,
};
use crate::pipeline::Algorithm;
use crate::pwl::Pwl;
use crate::stats::Statistics;

/// The sensor's noise profile at unity gain: noise ≈ constant + slope × √level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseTuning {
    /// Constant part (16-bit scale).
    pub reference_constant: f64,
    /// Slope.
    pub reference_slope: f64,
}

impl Default for NoiseTuning {
    /// The Raspberry Pi IPA's assumption without a profile (slope 3).
    fn default() -> Self {
        Self {
            reference_constant: 0.0,
            reference_slope: 3.0,
        }
    }
}

/// Spatial denoise (`rpi.denoise.sdn`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SdnTuning {
    /// Noise multiple treated as noise, with temporal denoise running.
    pub deviation: f64,
    /// Strength (1 − the proportion of the original let through), with temporal denoise.
    pub strength: f64,
    /// Second noise multiple (the second filter stage).
    pub deviation2: f64,
    /// Noise multiple without temporal denoise.
    pub deviation_no_tdn: f64,
    /// Strength without temporal denoise.
    pub strength_no_tdn: f64,
    /// Per-run factor by which the no-TDN values give way to the TDN ones.
    pub backoff: f64,
}

impl Default for SdnTuning {
    fn default() -> Self {
        Self {
            deviation: 3.2,
            strength: 0.25,
            deviation2: 3.2,
            deviation_no_tdn: 3.2,
            strength_no_tdn: 0.25,
            backoff: 0.75,
        }
    }
}

/// Colour denoise (`rpi.denoise.cdn`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CdnTuning {
    /// Noise multiple without temporal denoise.
    pub deviation: f64,
    /// Noise multiple with temporal denoise (a third of `deviation` if not tuned).
    pub deviation_with_tdn: Option<f64>,
    /// IIR strength.
    pub strength: f64,
}

impl Default for CdnTuning {
    fn default() -> Self {
        Self {
            deviation: 150.0,
            deviation_with_tdn: None,
            strength: 0.2,
        }
    }
}

/// Temporal denoise (`rpi.denoise.tdn`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TdnTuning {
    /// Noise multiple.
    pub deviation: f64,
    /// Threshold (a fraction).
    pub threshold: f64,
}

impl Default for TdnTuning {
    fn default() -> Self {
        Self {
            deviation: 0.5,
            threshold: 0.75,
        }
    }
}

/// Green equalisation (`rpi.geq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeqTuning {
    /// Offset (16-bit scale).
    pub offset: f64,
    /// Slope.
    pub slope: f64,
    /// Strength by lux (1 if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength: Option<Pwl>,
}

/// Sharpening (`rpi.sharpen`): factors on the ISP's default sharpening.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SharpenTuning {
    /// Threshold factor (higher: less sharpening of small detail and noise).
    pub threshold: f64,
    /// Strength factor.
    pub strength: f64,
    /// Limit factor.
    pub limit: f64,
}

impl Default for SharpenTuning {
    fn default() -> Self {
        Self {
            threshold: 1.0,
            strength: 1.0,
            limit: 1.0,
        }
    }
}

/// Everything this module tunes. Absent parts are off (sharpening: the ISP's default).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DenoiseTuning {
    /// Noise profile.
    pub noise: NoiseTuning,
    /// Spatial denoise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdn: Option<SdnTuning>,
    /// Colour denoise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn: Option<CdnTuning>,
    /// Temporal denoise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tdn: Option<TdnTuning>,
    /// Green equalisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geq: Option<GeqTuning>,
    /// Defective pixel correction: 0 off, 1 normal, 2 strong.
    pub dpc: u8,
    /// Sharpening.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpen: Option<SharpenTuning>,
}

impl DenoiseTuning {
    /// Check the values are usable.
    pub fn validate(&self) -> Result<()> {
        let err = |m: &str| Err(AlgoError::Tuning(format!("denoise: {m}")));
        if self.dpc > 2 {
            return err("dpc must be 0, 1 or 2");
        }
        if let Some(s) = &self.sdn
            && !(0.0..=1.0).contains(&s.backoff)
        {
            return err("sdn backoff must be in [0, 1]");
        }
        let n = &self.noise;
        if !(n.reference_constant >= 0.0 && n.reference_slope >= 0.0) {
            return err("noise reference values must be >= 0");
        }
        Ok(())
    }
}

/// The algorithm. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Denoise {
    tuning: DenoiseTuning,
    /// Temporal denoise runs (else the "no TDN" values throughout).
    temporal: bool,
    sdn_deviation: f64,
    sdn_strength: f64,
    sdn_deviation2: f64,
    cdn_deviation: f64,
}

impl Denoise {
    /// The algorithm for a tuning.
    pub fn new(tuning: DenoiseTuning) -> Result<Self> {
        tuning.validate()?;
        let mut d = Self {
            tuning,
            temporal: false,
            sdn_deviation: 0.0,
            sdn_strength: 0.0,
            sdn_deviation2: 0.0,
            cdn_deviation: 0.0,
        };
        d.reset();
        Ok(d)
    }

    fn reset(&mut self) {
        let sdn = self.tuning.sdn.unwrap_or_default();
        self.sdn_deviation = sdn.deviation_no_tdn;
        self.sdn_strength = sdn.strength_no_tdn;
        self.sdn_deviation2 = sdn.deviation_no_tdn;
        self.cdn_deviation = self.tuning.cdn.unwrap_or_default().deviation;
    }

    /// The tuned values the backoff ends at: the TDN ones while temporal denoise runs.
    fn targets(&self) -> (f64, f64, f64, f64) {
        let sdn = self.tuning.sdn.unwrap_or_default();
        let cdn = self.tuning.cdn.unwrap_or_default();
        if self.temporal {
            (
                sdn.deviation,
                sdn.strength,
                sdn.deviation2,
                cdn.deviation_with_tdn.unwrap_or(cdn.deviation / 3.0),
            )
        } else {
            (
                sdn.deviation_no_tdn,
                sdn.strength_no_tdn,
                sdn.deviation_no_tdn,
                cdn.deviation,
            )
        }
    }
}

impl Algorithm for Denoise {
    fn name(&self) -> &'static str {
        "denoise"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        // A new mode restarts the temporal average.
        self.temporal = config.temporal_denoise && self.tuning.tdn.is_some();
        self.reset();
        Ok(())
    }

    fn process(&mut self, _stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        let t = &self.tuning;
        let gain = meta.analogue_gain.max(1.0);
        let factor = gain.sqrt();
        let (constant, slope) = (
            t.noise.reference_constant * factor,
            t.noise.reference_slope * factor,
        );
        let (sdn_dev, sdn_str, sdn_dev2, cdn_dev) = self.targets();
        let backoff = t.sdn.map_or(0.75, |s| s.backoff);
        let mut out = DenoiseParams {
            noise_constant: constant,
            noise_slope: slope,
            ..DenoiseParams::default()
        };
        if t.sdn.is_some() {
            out.sdn = Some(SdnParams {
                noise_constant: constant * self.sdn_deviation,
                noise_slope: slope * self.sdn_deviation,
                // As the original: the second constant uses the tuned deviation2.
                noise_constant2: constant * sdn_dev2,
                noise_slope2: slope * self.sdn_deviation2,
                strength: self.sdn_strength,
            });
            let f = backoff;
            self.sdn_deviation = f * self.sdn_deviation + (1.0 - f) * sdn_dev;
            self.sdn_strength = f * self.sdn_strength + (1.0 - f) * sdn_str;
            self.sdn_deviation2 = f * self.sdn_deviation2 + (1.0 - f) * sdn_dev2;
        }
        if let (Some(tdn), true) = (t.tdn, self.temporal) {
            out.tdn = Some(TdnParams {
                noise_constant: constant * tdn.deviation,
                noise_slope: slope * tdn.deviation,
                threshold: tdn.threshold,
            });
        }
        if let Some(cdn) = t.cdn {
            out.cdn = Some(CdnParams {
                threshold: self.cdn_deviation * slope + constant,
                strength: cdn.strength,
            });
            let f = backoff;
            self.cdn_deviation = f * self.cdn_deviation + (1.0 - f) * cdn_dev;
        }
        if let Some(geq) = &t.geq {
            let lux = meta.lux.unwrap_or(params.lux);
            let strength = geq.strength.as_ref().map_or(1.0, |p| p.eval_clamped(lux)) * gain;
            out.geq = Some(GeqParams {
                offset: (geq.offset * strength).clamp(0.0, 65535.0),
                slope: (geq.slope * strength).clamp(0.0, 0.99999),
            });
        }
        out.dpc = t.dpc;
        params.denoise = out;
        params.sharpen = t.sharpen.map(|s| SharpenParams {
            threshold: s.threshold,
            strength: s.strength,
            limit: s.limit,
        });
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    fn tuning() -> DenoiseTuning {
        DenoiseTuning {
            noise: NoiseTuning {
                reference_constant: 10.0,
                reference_slope: 4.0,
            },
            sdn: Some(SdnTuning {
                deviation: 0.6,
                strength: 0.8,
                deviation2: 3.0,
                deviation_no_tdn: 3.2,
                strength_no_tdn: 0.8,
                backoff: 0.75,
            }),
            cdn: Some(CdnTuning {
                deviation: 200.0,
                deviation_with_tdn: None,
                strength: 0.2,
            }),
            tdn: Some(TdnTuning {
                deviation: 1.0,
                threshold: 0.1,
            }),
            geq: Some(GeqTuning {
                offset: 200.0,
                slope: 0.01,
                strength: None,
            }),
            dpc: 1,
            sharpen: Some(SharpenTuning {
                threshold: 0.5,
                strength: 1.5,
                limit: 0.8,
            }),
        }
    }

    fn meta(gain: f64) -> FrameMetadata {
        FrameMetadata::new(
            1,
            Duration::from_millis(10),
            gain,
            Duration::from_millis(33),
        )
    }

    #[test]
    fn strengths_follow_the_gain() {
        let mut d = Denoise::new(tuning()).unwrap();
        d.prepare(&CameraConfig::default()).unwrap();
        let mut p = Params::default();
        d.process(&Statistics::default(), &meta(4.0), &mut p);
        let n = &p.denoise;
        assert_eq!((n.noise_constant, n.noise_slope), (20.0, 8.0));
        let sdn = n.sdn.unwrap();
        assert_eq!(sdn.noise_slope, 8.0 * 3.2);
        assert_eq!(n.cdn.unwrap().threshold, 200.0 * 8.0 + 20.0);
        assert_eq!(n.geq.unwrap().offset, 800.0);
        assert_eq!(n.dpc, 1);
        assert_eq!(p.sharpen.unwrap().strength, 1.5);
        // No temporal denoise in this configuration: nothing backs off.
        assert!(n.tdn.is_none());
        d.process(&Statistics::default(), &meta(4.0), &mut p);
        assert_eq!(p.denoise.sdn.unwrap().noise_slope, 8.0 * 3.2);
    }

    #[test]
    fn spatial_denoise_backs_off_while_temporal_builds_up() {
        let mut d = Denoise::new(tuning()).unwrap();
        let config = CameraConfig {
            temporal_denoise: true,
            ..CameraConfig::default()
        };
        d.prepare(&config).unwrap();
        let mut p = Params::default();
        let mut slopes = Vec::new();
        for _ in 0..30 {
            d.process(&Statistics::default(), &meta(1.0), &mut p);
            slopes.push(p.denoise.sdn.unwrap().noise_slope);
        }
        assert_eq!(slopes[0], 4.0 * 3.2);
        assert!(slopes.windows(2).all(|w| w[1] < w[0]));
        assert!((slopes[29] - 4.0 * 0.6).abs() < 0.01, "{}", slopes[29]);
        assert_eq!(p.denoise.tdn.unwrap().noise_slope, 4.0);
        let cdn = p.denoise.cdn.unwrap().threshold;
        assert!((cdn - (200.0 / 3.0 * 4.0 + 10.0)).abs() < 0.5, "{cdn}");
        // A new mode starts over.
        d.prepare(&config).unwrap();
        d.process(&Statistics::default(), &meta(1.0), &mut p);
        assert_eq!(p.denoise.sdn.unwrap().noise_slope, 4.0 * 3.2);
    }
}
