//! Black level: per-channel levels from the tuning, else the sensor description's.
//!
//! Follows Raspberry Pi's `black_level.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi
//! Ltd), with levels normalised to full scale 1.0 (the Raspberry Pi files use 16 bits).

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::Result;
use crate::frame::FrameMetadata;
use crate::params::{BlackLevels, Params};
use crate::pipeline::Algorithm;
use crate::stats::Statistics;

/// Black level tuning, normalised.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlackLevelTuning {
    /// Red.
    pub r: f64,
    /// Green.
    pub g: f64,
    /// Blue.
    pub b: f64,
}

impl Default for BlackLevelTuning {
    /// 64 in 10 bits.
    fn default() -> Self {
        let v = 4096.0 / 65536.0;
        Self { r: v, g: v, b: v }
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
        let t = self.tuning.unwrap_or_else(|| match config.black_level {
            Some(v) => BlackLevelTuning { r: v, g: v, b: v },
            None => BlackLevelTuning::default(),
        });
        self.levels = BlackLevels {
            r: t.r,
            g: t.g,
            b: t.b,
        };
        Ok(())
    }

    fn initial(&self, params: &mut Params) {
        params.black_level = self.levels;
    }

    fn process(&mut self, _: &Statistics, _: &FrameMetadata, params: &mut Params) {
        params.black_level = self.levels;
    }
}
