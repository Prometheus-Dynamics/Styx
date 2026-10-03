//! CCM: colour correction matrix interpolated by colour temperature, with saturation.
//!
//! Ported from Raspberry Pi's `ccm.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi Ltd).

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
use crate::params::{IDENTITY, Matrix3, Params, mat_mul};
use crate::pipeline::Algorithm;
use crate::pwl::Pwl;
use crate::stats::Statistics;

/// A matrix calibrated at a colour temperature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CtCcm {
    /// Colour temperature (K).
    pub ct: f64,
    /// Row-major matrix.
    pub ccm: Matrix3,
}

/// CCM tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CcmTuning {
    /// Matrices in increasing colour temperature.
    pub ccms: Vec<CtCcm>,
    /// Saturation factor by lux.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation: Option<Pwl>,
}

impl Default for CcmTuning {
    fn default() -> Self {
        Self {
            ccms: vec![CtCcm {
                ct: 4000.0,
                ccm: IDENTITY,
            }],
            saturation: None,
        }
    }
}

impl CcmTuning {
    /// Check the tuning.
    pub fn validate(&self) -> Result<()> {
        if self.ccms.is_empty() {
            return Err(AlgoError::tuning("ccm: no matrices"));
        }
        if self.ccms.windows(2).any(|w| w[1].ct <= w[0].ct) {
            return Err(AlgoError::tuning("ccm: matrices must be in increasing ct"));
        }
        Ok(())
    }

    /// The matrix for a colour temperature (linear interpolation, clamped at the ends).
    pub fn matrix_for(&self, ct: f64) -> Matrix3 {
        let (first, last) = (&self.ccms[0], &self.ccms[self.ccms.len() - 1]);
        if ct <= first.ct {
            return first.ccm;
        }
        if ct >= last.ct {
            return last.ccm;
        }
        let i = self.ccms.iter().position(|c| c.ct >= ct).unwrap_or(1);
        let (a, b) = (&self.ccms[i - 1], &self.ccms[i]);
        let l = (ct - a.ct) / (b.ct - a.ct);
        std::array::from_fn(|k| l * b.ccm[k] + (1.0 - l) * a.ccm[k])
    }
}

/// Scale the chroma of a matrix's output: `Y2RGB × diag(1, s, s) × RGB2Y × ccm`.
pub fn apply_saturation(ccm: &Matrix3, saturation: f64) -> Matrix3 {
    const RGB2Y: Matrix3 = [
        0.299, 0.587, 0.114, -0.169, -0.331, 0.500, 0.500, -0.419, -0.081,
    ];
    const Y2RGB: Matrix3 = [
        1.000, 0.000, 1.402, 1.000, -0.345, -0.714, 1.000, 1.771, 0.000,
    ];
    let s = [1.0, 0.0, 0.0, 0.0, saturation, 0.0, 0.0, 0.0, saturation];
    mat_mul(&Y2RGB, &mat_mul(&s, &mat_mul(&RGB2Y, ccm)))
}

/// The CCM algorithm.
#[derive(Debug, Clone)]
pub struct Ccm {
    tuning: CcmTuning,
}

impl Ccm {
    /// A CCM algorithm.
    pub fn new(tuning: CcmTuning) -> Result<Self> {
        tuning.validate()?;
        Ok(Self { tuning })
    }
}

impl Algorithm for Ccm {
    fn name(&self) -> &'static str {
        "ccm"
    }

    fn prepare(&mut self, _: &CameraConfig) -> Result<()> {
        Ok(())
    }

    fn initial(&self, params: &mut Params) {
        params.ccm = self.tuning.matrix_for(params.colour_temperature);
    }

    fn process(&mut self, _: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        let mut sat = meta.controls.saturation;
        if let Some(s) = &self.tuning.saturation {
            sat *= s.eval_clamped(meta.lux.unwrap_or(params.lux));
        }
        let m = apply_saturation(&self.tuning.matrix_for(params.colour_temperature), sat);
        params.ccm = m.map(|v| v.clamp(-8.0, 7.9999));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_by_ct() {
        let t = CcmTuning {
            ccms: vec![
                CtCcm {
                    ct: 3000.0,
                    ccm: [2.0; 9],
                },
                CtCcm {
                    ct: 5000.0,
                    ccm: [4.0; 9],
                },
            ],
            saturation: None,
        };
        assert_eq!(t.matrix_for(4000.0), [3.0; 9]);
        assert_eq!(t.matrix_for(1000.0), [2.0; 9]);
        assert_eq!(t.matrix_for(9000.0), [4.0; 9]);
    }

    #[test]
    fn saturation_one_is_nearly_identity_and_zero_is_grey() {
        let m = apply_saturation(&IDENTITY, 1.0);
        for (a, b) in m.iter().zip(IDENTITY.iter()) {
            assert!((a - b).abs() < 2e-3, "{m:?}");
        }
        let g = apply_saturation(&IDENTITY, 0.0);
        // Every output row is the luma row.
        for r in 0..3 {
            assert!((g[r * 3] - 0.299).abs() < 1e-9);
        }
    }
}
