//! AGC tuning. Field names and defaults follow Raspberry Pi's `rpi.agc` (channel 0), with
//! times in microseconds and luma targets normalised to 1.0.

use alloc::collections::BTreeMap;
use alloc::{format, string::String, vec, vec::Vec};

use serde::{Deserialize, Serialize};

use crate::error::{AlgoError, Result};
use crate::pwl::Pwl;

/// Metering weights for one mode.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeteringMode {
    /// Weights per zone, row-major.
    pub weights: Vec<f64>,
    /// The weight grid (width, height). When absent: the statistics grid if the count matches,
    /// else a square grid if the count is a square number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid: Option<(u32, u32)>,
}

/// How exposure is split between time and gain: exposure time rises to `exposure_us[i]`,
/// then gain to `gain[i]`, stage by stage.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposureProfile {
    /// Exposure time per stage, microseconds.
    pub exposure_us: Vec<f64>,
    /// Gain per stage.
    pub gain: Vec<f64>,
}

/// Which way a constraint pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bound {
    /// The metered part must reach at least the target (raises exposure).
    Lower,
    /// The metered part must not exceed the target (lowers exposure).
    Upper,
}

/// A histogram constraint: the mean between two quantiles must meet a target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    /// Direction.
    pub bound: Bound,
    /// Lower quantile.
    pub q_lo: f64,
    /// Upper quantile.
    pub q_hi: f64,
    /// Target luma by lux.
    pub y_target: Pwl,
}

/// AGC tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgcTuning {
    /// Metering modes by name. Names without weights here use built-in weights for
    /// "centre-weighted", "spot" and "matrix"/"average".
    pub metering_modes: BTreeMap<String, MeteringMode>,
    /// The metering mode used unless the application picks one.
    pub default_metering_mode: String,
    /// Exposure profiles by name.
    pub exposure_modes: BTreeMap<String, ExposureProfile>,
    /// The default exposure profile.
    pub default_exposure_mode: String,
    /// Constraint modes by name.
    pub constraint_modes: BTreeMap<String, Vec<Constraint>>,
    /// The default constraint mode.
    pub default_constraint_mode: String,
    /// Target mean luma by lux.
    pub y_target: Pwl,
    /// Damping: fraction of the way to the target moved per frame, for changes up to
    /// `full_step`.
    pub speed: f64,
    /// Relative change of total exposure above which AE moves straight to the target
    /// (undamped) at any time: the control delays tell when it lands, so a large step needs no
    /// damping. 0 damps every change after start-up (as Raspberry Pi's AGC does).
    pub full_step: f64,
    /// Frames at start-up that move straight to the target.
    pub startup_frames: u32,
    /// Frames an application should expect convergence to take (informational).
    pub convergence_frames: u32,
    /// When saturated and the target is above this, cut exposure by this factor at once.
    pub fast_reduce_threshold: f64,
    /// Exposure compensation factor always applied.
    pub base_ev: f64,
    /// Start-up exposure time, microseconds.
    pub default_exposure_us: f64,
    /// Start-up analogue gain.
    pub default_analogue_gain: f64,
    /// Relative change of total exposure below which nothing is changed.
    pub stable_region: f64,
    /// Allow fast de-saturation.
    pub desaturate: bool,
    /// Largest digital gain AGC may use.
    pub max_digital_gain: f64,
}

fn pwl(v: &[f64]) -> Pwl {
    Pwl::from_flat(v).unwrap_or_default()
}

impl Default for AgcTuning {
    fn default() -> Self {
        let lower = |q_lo, q_hi, y| Constraint {
            bound: Bound::Lower,
            q_lo,
            q_hi,
            y_target: Pwl::constant(y),
        };
        let upper = Constraint {
            bound: Bound::Upper,
            q_lo: 0.98,
            q_hi: 1.0,
            y_target: Pwl::constant(0.8),
        };
        let profile = |e: &[f64], g: &[f64]| ExposureProfile {
            exposure_us: e.to_vec(),
            gain: g.to_vec(),
        };
        let gains = [1.0, 2.0, 4.0, 6.0, 8.0];
        Self {
            metering_modes: BTreeMap::new(),
            default_metering_mode: "centre-weighted".into(),
            exposure_modes: BTreeMap::from([
                (
                    "normal".into(),
                    profile(&[100.0, 10000.0, 30000.0, 60000.0, 66666.0], &gains),
                ),
                (
                    "short".into(),
                    profile(&[100.0, 5000.0, 10000.0, 20000.0, 33333.0], &gains),
                ),
                (
                    "long".into(),
                    profile(&[100.0, 10000.0, 30000.0, 60000.0, 120000.0], &gains),
                ),
            ]),
            default_exposure_mode: "normal".into(),
            constraint_modes: BTreeMap::from([
                ("normal".into(), vec![lower(0.98, 1.0, 0.5)]),
                ("highlight".into(), vec![lower(0.98, 1.0, 0.5), upper]),
                ("shadows".into(), vec![lower(0.0, 0.5, 0.17)]),
            ]),
            default_constraint_mode: "normal".into(),
            y_target: pwl(&[0.0, 0.16, 1000.0, 0.165, 10000.0, 0.17]),
            speed: 0.2,
            full_step: 0.08,
            startup_frames: 10,
            convergence_frames: 6,
            fast_reduce_threshold: 0.4,
            base_ev: 1.0,
            default_exposure_us: 1000.0,
            default_analogue_gain: 1.0,
            stable_region: 0.02,
            desaturate: true,
            max_digital_gain: 4.0,
        }
    }
}

impl AgcTuning {
    /// Check the tuning is consistent.
    pub fn validate(&self) -> Result<()> {
        let err = |m: String| Err(AlgoError::Tuning(format!("agc: {m}")));
        // Names in quotes as `{:?}` gives them (without its escaping, which brings core's
        // Unicode tables into a firmware image).
        if self.y_target.is_empty() {
            return err("y_target is empty".into());
        }
        if !self
            .exposure_modes
            .contains_key(&self.default_exposure_mode)
        {
            return err(format!(
                "no exposure mode \"{}\"",
                self.default_exposure_mode
            ));
        }
        if !self
            .constraint_modes
            .contains_key(&self.default_constraint_mode)
        {
            return err(format!(
                "no constraint mode \"{}\"",
                self.default_constraint_mode
            ));
        }
        for (name, p) in &self.exposure_modes {
            if p.exposure_us.len() < 2 || p.exposure_us.len() != p.gain.len() {
                return err(format!(
                    "exposure mode \"{name}\" needs at least two stages and as many gains as times"
                ));
            }
            if p.exposure_us
                .iter()
                .chain(&p.gain)
                .any(|v| v.partial_cmp(&0.0) != Some(core::cmp::Ordering::Greater))
            {
                return err(format!("exposure mode \"{name}\" has a non-positive value"));
            }
        }
        for (name, m) in &self.metering_modes {
            if m.weights.is_empty() || m.weights.iter().any(|w| w.is_nan() || *w < 0.0) {
                return err(format!(
                    "metering mode \"{name}\" needs non-negative weights"
                ));
            }
            if let Some((w, h)) = m.grid
                && (w * h) as usize != m.weights.len()
            {
                return err(format!(
                    "metering mode \"{name}\": grid does not match weights"
                ));
            }
        }
        for (name, cs) in &self.constraint_modes {
            for c in cs {
                if !(0.0..=1.0).contains(&c.q_lo) || !(c.q_lo..=1.0).contains(&c.q_hi) {
                    return err(format!("constraint mode \"{name}\": bad quantiles"));
                }
            }
        }
        if !(self.speed > 0.0 && self.speed <= 1.0) {
            return err("speed must be in (0, 1]".into());
        }
        if !(self.full_step >= 0.0 && self.full_step.is_finite()) {
            return err("full_step must be finite and not negative".into());
        }
        if self.max_digital_gain.is_nan()
            || self.max_digital_gain < 1.0
            || self.default_exposure_us.is_nan()
            || self.default_exposure_us <= 0.0
        {
            return err("max_digital_gain >= 1 and default_exposure_us > 0 required".into());
        }
        Ok(())
    }
}
