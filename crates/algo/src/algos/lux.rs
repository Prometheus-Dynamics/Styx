//! Lux: scene illuminance from the image brightness and the exposure that produced it.
//!
//! Ported from Raspberry Pi's `lux.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi Ltd).
//! `lux = ref_lux × (ref_exposure / exposure) × (ref_gain / gain) × (Y / ref_Y) / sensitivity`.
//! A lux value in the frame metadata (e.g. from a light sensor) takes precedence.

// The tuning types are always there; the algorithm needs feature `lux` (on with `std`).
#![cfg_attr(not(feature = "lux"), allow(dead_code, unused_imports))]

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
use crate::params::Params;
use crate::pipeline::Algorithm;
use crate::stats::Statistics;

/// Lux calibration: one reference image.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LuxTuning {
    /// Exposure of the reference image, microseconds.
    pub reference_exposure_us: f64,
    /// Analogue gain of the reference image.
    pub reference_gain: f64,
    /// Aperture of the reference image.
    #[serde(default = "one")]
    pub reference_aperture: f64,
    /// Illuminance of the reference scene.
    pub reference_lux: f64,
    /// Mean luma of the reference image, normalised.
    pub reference_y: f64,
}

fn one() -> f64 {
    1.0
}

impl LuxTuning {
    /// Check the tuning.
    pub fn validate(&self) -> Result<()> {
        let v = [
            self.reference_exposure_us,
            self.reference_gain,
            self.reference_aperture,
            self.reference_lux,
            self.reference_y,
        ];
        if v.iter().all(|x| *x > 0.0 && x.is_finite()) {
            Ok(())
        } else {
            Err(AlgoError::tuning("lux: reference values must be positive"))
        }
    }
}

#[cfg(feature = "lux")]
/// The lux estimator.
#[derive(Debug, Clone)]
pub struct Lux {
    tuning: LuxTuning,
    sensitivity: f64,
}

#[cfg(feature = "lux")]
impl Lux {
    /// A lux estimator.
    pub fn new(tuning: LuxTuning) -> Self {
        Self {
            tuning,
            sensitivity: 1.0,
        }
    }

    /// The estimate for a frame.
    pub fn estimate(&self, mean_y: f64, meta: &FrameMetadata) -> f64 {
        let t = &self.tuning;
        let exposure = meta.exposure.as_secs_f64().max(1e-9);
        let gain = (meta.analogue_gain * meta.digital_gain).max(1e-9);
        t.reference_lux
            * (t.reference_exposure_us * 1e-6 / exposure)
            * (t.reference_gain / gain)
            * (mean_y / t.reference_y)
            / self.sensitivity
    }
}

#[cfg(feature = "lux")]
impl Algorithm for Lux {
    fn name(&self) -> &'static str {
        "lux"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        self.tuning.validate()?;
        self.sensitivity = config.sensitivity;
        Ok(())
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        params.lux = meta
            .lux
            .unwrap_or_else(|| self.estimate(stats.mean_luma(), meta));
    }
}
