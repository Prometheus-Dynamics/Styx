//! AWB tuning. Field names and defaults follow Raspberry Pi's `rpi.awb`; `min_g` is normalised
//! to full scale 1.0.

use alloc::{format, string::String, vec::Vec};

use alloc::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{AlgoError, Result};
use crate::pwl::Pwl;

/// Prior log likelihood of each colour temperature at a lux level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwbPrior {
    /// Lux level.
    pub lux: f64,
    /// Log likelihood by colour temperature.
    pub prior: Pwl,
}

/// The colour temperature range searched in a mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwbMode {
    /// Lowest temperature (K).
    pub lo: f64,
    /// Highest temperature (K).
    pub hi: f64,
}

/// AWB tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AwbTuning {
    /// Use the Bayesian colour-temperature search (needs `ct_curve`, `priors` and `modes`);
    /// otherwise grey world.
    pub bayes: bool,
    /// Points `[ct, r, b]`: the sensor's R/G and B/G of a grey under each temperature.
    pub ct_curve: Vec<[f64; 3]>,
    /// Priors, in increasing lux.
    pub priors: Vec<AwbPrior>,
    /// Modes by name.
    pub modes: BTreeMap<String, AwbMode>,
    /// The mode used unless the application picks one.
    pub default_mode: String,
    /// Frames between estimates once started.
    pub frame_period: u32,
    /// Frames at start-up estimated every frame and applied without damping.
    pub startup_frames: u32,
    /// Frames an application should expect convergence to take (informational).
    pub convergence_frames: u32,
    /// Damping: fraction of the way to the estimate moved per frame.
    pub speed: f64,
    /// Minimum pixels counted for a zone to be used.
    pub min_pixels: f64,
    /// Minimum mean green for a zone to be used.
    pub min_g: f64,
    /// Estimate only with more than this many usable zones.
    pub min_regions: u32,
    /// Coarse search step, as a fraction of temperature / 10.
    pub coarse_step: f64,
    /// Offset of the aimed-for white point in r.
    pub whitepoint_r: f64,
    /// Offset of the aimed-for white point in b.
    pub whitepoint_b: f64,
    /// Proportion of each zone's pixels added as grey under `bias_ct`.
    pub bias_proportion: f64,
    /// Bias colour temperature.
    pub bias_ct: f64,
    /// Cap on each zone's squared colour error.
    pub delta_limit: f64,
    /// How far the fine search may leave the curve on the positive side.
    pub transverse_pos: f64,
    /// ... and on the negative side.
    pub transverse_neg: f64,
    /// Red sensitivity of this sensor relative to the tuned one.
    pub sensitivity_r: f64,
    /// Blue sensitivity relative to the tuned one.
    pub sensitivity_b: f64,
    /// Styx: the search weights its points by `exp(-(cost - best) / softness)` (cost in the
    /// search's log likelihood units) instead of taking the best: when two separate
    /// temperatures fit almost equally well the estimate moves between them continuously
    /// instead of jumping. 0 takes the best (Raspberry Pi's behaviour).
    pub softness: f64,
    /// Styx: log likelihood cost of leaving the temperature currently applied (hysteresis).
    /// A different temperature has to fit better by about this much before the estimate moves
    /// to it; a clear change of the light (a deep new minimum) moves at once. 0 disables it.
    pub hysteresis: f64,
    /// Styx: width of the hysteresis well in mired (1e6 / K).
    pub hysteresis_mired: f64,
}

impl Default for AwbTuning {
    fn default() -> Self {
        Self {
            bayes: true,
            ct_curve: Vec::new(),
            priors: Vec::new(),
            modes: BTreeMap::new(),
            default_mode: "auto".into(),
            frame_period: 10,
            startup_frames: 10,
            convergence_frames: 3,
            speed: 0.05,
            min_pixels: 16.0,
            min_g: 32.0 / 65536.0,
            min_regions: 10,
            coarse_step: 0.2,
            whitepoint_r: 0.0,
            whitepoint_b: 0.0,
            bias_proportion: 0.0,
            bias_ct: 4500.0,
            delta_limit: 0.2,
            transverse_pos: 0.01,
            transverse_neg: 0.01,
            sensitivity_r: 1.0,
            sensitivity_b: 1.0,
            softness: 0.2,
            hysteresis: 2.0,
            hysteresis_mired: 25.0,
        }
    }
}

/// The CT curve split into r(ct), b(ct) and their inverses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CtCurve {
    pub r: Pwl,
    pub b: Pwl,
    pub r_inv: Option<Pwl>,
    pub b_inv: Option<Pwl>,
}

impl AwbTuning {
    /// The CT curve, if there is one.
    pub(crate) fn curve(&self) -> Result<Option<CtCurve>> {
        if self.ct_curve.is_empty() {
            return Ok(None);
        }
        let r = Pwl::new(self.ct_curve.iter().map(|p| (p[0], p[1])).collect())?;
        let b = Pwl::new(self.ct_curve.iter().map(|p| (p[0], p[2])).collect())?;
        Ok(Some(CtCurve {
            r_inv: r.inverse(),
            b_inv: b.inverse(),
            r,
            b,
        }))
    }

    /// Whether the Bayesian search is usable (else grey world).
    pub fn uses_bayes(&self) -> bool {
        self.bayes
            && self.ct_curve.len() >= 2
            && !self.priors.is_empty()
            && self.modes.contains_key(&self.default_mode)
    }

    /// Check the tuning is consistent.
    pub fn validate(&self) -> Result<()> {
        let err = |m: &str| Err(AlgoError::Tuning(format!("awb: {m}")));
        if !self.ct_curve.is_empty() && self.ct_curve.len() < 2 {
            return err("ct_curve needs at least two points");
        }
        if self.ct_curve.iter().any(|p| !(p[1] > 0.0 && p[2] > 0.0)) {
            return err("ct_curve r and b must be positive");
        }
        self.curve()?;
        if self.priors.windows(2).any(|w| w[1].lux <= w[0].lux) {
            return err("priors must be in increasing lux");
        }
        if self.modes.values().any(|m| !(m.lo > 0.0 && m.lo <= m.hi)) {
            return err("modes need 0 < lo <= hi");
        }
        if !(self.speed > 0.0 && self.speed <= 1.0) {
            return err("speed must be in (0, 1]");
        }
        if !(self.transverse_pos > 0.0 && self.transverse_neg > 0.0) {
            return err("transverse_pos and transverse_neg must be positive");
        }
        if !(self.softness >= 0.0 && self.hysteresis >= 0.0 && self.hysteresis_mired > 0.0) {
            return err("softness and hysteresis must be >= 0, hysteresis_mired > 0");
        }
        if self.coarse_step.is_nan() || self.coarse_step <= 0.0 {
            return err("coarse_step must be positive");
        }
        Ok(())
    }
}
