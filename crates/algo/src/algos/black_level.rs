//! Black level: per-channel levels from the tuning, else the sensor description's.
//!
//! Follows Raspberry Pi's `black_level.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi
//! Ltd), with levels normalised to full scale 1.0 (the Raspberry Pi files use 16 bits).
//!
//! Styx addition: a sensor whose black level moves with the analogue gain can list levels
//! measured at several gains (`by_gain`, written by `styx-tune` from dark frames); each frame
//! then gets the levels interpolated at the gain that produced it.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
use crate::params::{BlackLevels, Params};
use crate::pipeline::Algorithm;
use crate::stats::Statistics;

/// Black levels measured at one analogue gain, normalised.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GainBlackLevel {
    /// Analogue gain.
    pub gain: f64,
    /// Red.
    pub r: f64,
    /// Green.
    pub g: f64,
    /// Blue.
    pub b: f64,
}

/// Black level tuning, normalised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlackLevelTuning {
    /// Red.
    pub r: f64,
    /// Green.
    pub g: f64,
    /// Blue.
    pub b: f64,
    /// Styx: levels by analogue gain, in increasing gain. When present they replace `r`, `g`,
    /// `b` (which stay the levels for tools that read one value): linear in gain between the
    /// entries, the nearest entry outside them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_gain: Vec<GainBlackLevel>,
}

impl Default for BlackLevelTuning {
    /// 64 in 10 bits.
    fn default() -> Self {
        let v = 4096.0 / 65536.0;
        Self {
            r: v,
            g: v,
            b: v,
            by_gain: Vec::new(),
        }
    }
}

impl BlackLevelTuning {
    /// One level for every channel.
    pub fn uniform(v: f64) -> Self {
        Self {
            r: v,
            g: v,
            b: v,
            by_gain: Vec::new(),
        }
    }

    /// Check the tuning.
    pub fn validate(&self) -> Result<()> {
        let ok = |v: f64| (0.0..1.0).contains(&v);
        if ![self.r, self.g, self.b].into_iter().all(ok)
            || !self.by_gain.iter().all(|l| ok(l.r) && ok(l.g) && ok(l.b))
        {
            return Err(AlgoError::tuning("black_level: levels must be in [0, 1)"));
        }
        if self
            .by_gain
            .iter()
            .any(|l| l.gain.is_nan() || l.gain <= 0.0)
            || self.by_gain.windows(2).any(|w| w[1].gain <= w[0].gain)
        {
            return Err(AlgoError::tuning(
                "black_level: by_gain must be in increasing positive gain",
            ));
        }
        Ok(())
    }

    /// The levels at an analogue gain.
    pub fn at_gain(&self, gain: f64) -> BlackLevels {
        let l = &self.by_gain;
        let pick = |e: &GainBlackLevel| BlackLevels {
            r: e.r,
            g: e.g,
            b: e.b,
        };
        match l.iter().position(|e| e.gain >= gain) {
            _ if l.is_empty() => BlackLevels {
                r: self.r,
                g: self.g,
                b: self.b,
            },
            Some(0) => pick(&l[0]),
            None => pick(&l[l.len() - 1]),
            Some(i) => {
                let (a, b) = (&l[i - 1], &l[i]);
                let t = (gain - a.gain) / (b.gain - a.gain);
                let mix = |x: f64, y: f64| x + (y - x) * t;
                BlackLevels {
                    r: mix(a.r, b.r),
                    g: mix(a.g, b.g),
                    b: mix(a.b, b.b),
                }
            }
        }
    }
}

/// The black level algorithm.
#[derive(Debug, Clone, Default)]
pub struct BlackLevel {
    tuning: Option<BlackLevelTuning>,
    levels: BlackLevels,
}

impl BlackLevel {
    /// Levels from the tuning, or (when `None`) from the camera configuration.
    pub fn new(tuning: Option<BlackLevelTuning>) -> Self {
        Self {
            tuning,
            levels: BlackLevels::default(),
        }
    }
}

impl Algorithm for BlackLevel {
    fn name(&self) -> &'static str {
        "black_level"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        if let Some(t) = &self.tuning {
            t.validate()?;
        }
        self.levels = match &self.tuning {
            Some(t) => t.at_gain(1.0),
            None => {
                let v = config.black_level.unwrap_or(BlackLevelTuning::default().r);
                BlackLevels { r: v, g: v, b: v }
            }
        };
        Ok(())
    }

    fn initial(&self, params: &mut Params) {
        params.black_level = self.levels;
    }

    fn process(&mut self, _: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        params.black_level = match &self.tuning {
            Some(t) if !t.by_gain.is_empty() => t.at_gain(meta.analogue_gain),
            _ => self.levels,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_gain() {
        let at = |gain, v| GainBlackLevel {
            gain,
            r: v,
            g: v,
            b: v,
        };
        let t = BlackLevelTuning {
            by_gain: vec![at(1.0, 0.06), at(4.0, 0.09)],
            ..BlackLevelTuning::uniform(0.06)
        };
        t.validate().unwrap();
        assert_eq!(t.at_gain(0.5).g, 0.06);
        assert!((t.at_gain(2.0).g - 0.07).abs() < 1e-12);
        assert_eq!(t.at_gain(8.0).r, 0.09);
        assert_eq!(BlackLevelTuning::uniform(0.05).at_gain(8.0).b, 0.05);
        let bad = BlackLevelTuning {
            by_gain: vec![at(4.0, 0.06), at(1.0, 0.06)],
            ..t
        };
        assert!(bad.validate().is_err());
    }
}
