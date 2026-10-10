//! Contrast: the tone (gamma) curve, with adaptive histogram stretch and manual
//! brightness/contrast.
//!
//! Ported from Raspberry Pi's `contrast.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi
//! Ltd), on normalised values (the Raspberry Pi files use 16 bits; the loader converts).

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use crate::params::Params;
use crate::pipeline::Algorithm;
use crate::pwl::{Pwl, PwlScratch};
use crate::stats::{Histogram, Statistics};

/// Contrast tuning, all levels normalised to 1.0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ContrastTuning {
    /// Adaptive contrast enhancement.
    pub ce_enable: bool,
    /// Quantile near the bottom of the histogram to move.
    pub lo_histogram: f64,
    /// Where to move it.
    pub lo_level: f64,
    /// By at most this much.
    pub lo_max: f64,
    /// Quantile near the top of the histogram to move.
    pub hi_histogram: f64,
    /// Where to move it.
    pub hi_level: f64,
    /// By at most this much.
    pub hi_max: f64,
    /// The base tone curve, `[0, 1] -> [0, 1]`.
    pub gamma_curve: Pwl,
}

/// An sRGB-like curve (linear toe, 1/2.4 power) sampled densely near black.
pub fn default_gamma() -> Pwl {
    let f = |x: f64| {
        if x <= 0.003_130_8 {
            12.92 * x
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        }
    };
    let mut xs: Vec<f64> = (0..16).map(|i| f64::from(i) / 256.0).collect();
    xs.extend((1..=32).map(|i| 0.0625 + (1.0 - 0.0625) * f64::from(i) / 32.0));
    Pwl::new(xs.into_iter().map(|x| (x, f(x))).collect()).unwrap_or_default()
}

impl Default for ContrastTuning {
    fn default() -> Self {
        Self {
            ce_enable: true,
            lo_histogram: 0.01,
            lo_level: 0.015,
            lo_max: 500.0 / 65536.0,
            hi_histogram: 0.95,
            hi_level: 0.95,
            hi_max: 2000.0 / 65536.0,
            gamma_curve: default_gamma(),
        }
    }
}

impl ContrastTuning {
    /// Check the tuning.
    pub fn validate(&self) -> Result<()> {
        if self.gamma_curve.is_empty() {
            return Err(AlgoError::tuning("contrast: empty gamma_curve"));
        }
        Ok(())
    }
}

/// The curve that pulls an empty histogram bottom down and top up, keeping the median.
#[cfg(test)]
fn stretch_curve(h: &Histogram, t: &ContrastTuning) -> Option<Pwl> {
    let mut out = Pwl::default();
    let mut points = Vec::new();
    stretch_into(h, t, &mut points, &mut out).then_some(out)
}

/// [`stretch_curve`] into `out`, with `points` as its breakpoints' scratch; `false` if the
/// curve is not valid (`out` is left as it was then).
fn stretch_into(
    h: &Histogram,
    t: &ContrastTuning,
    points: &mut Vec<(f64, f64)>,
    out: &mut Pwl,
) -> bool {
    let bins = h.len() as f64;
    let at = |q| h.quantile(q) / bins;
    let lo = at(t.lo_histogram)
        .min(t.lo_level + t.lo_max)
        .max(t.lo_level);
    let mid = at(0.5);
    let hi = at(t.hi_histogram)
        .max(t.hi_level - t.hi_max)
        .min(t.hi_level);
    let pts = [
        (0.0, 0.0),
        (lo, t.lo_level),
        (mid, mid),
        (hi, t.hi_level),
        (1.0, 1.0),
    ];
    points.clear();
    for p in pts {
        if points.last().is_none_or(|l| p.0 > l.0 + 1e-9) {
            points.push(p);
        }
    }
    out.set_points(points).is_ok()
}

/// The contrast algorithm.
#[derive(Debug, Clone)]
pub struct Contrast {
    tuning: ContrastTuning,
    /// Scratch for the stretch, reused every frame.
    points: Vec<(f64, f64)>,
    stretch: Pwl,
    scratch: PwlScratch,
}

impl Contrast {
    /// A contrast algorithm.
    pub fn new(tuning: ContrastTuning) -> Self {
        Self {
            tuning,
            points: Vec::new(),
            stretch: Pwl::default(),
            scratch: PwlScratch::default(),
        }
    }
}

impl Algorithm for Contrast {
    fn name(&self) -> &'static str {
        "contrast"
    }

    fn prepare(&mut self, _: &CameraConfig) -> Result<()> {
        self.tuning.validate()
    }

    fn initial(&self, params: &mut Params) {
        params.gamma = Some(self.tuning.gamma_curve.clone());
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        let Self {
            tuning: t,
            points,
            stretch,
            scratch,
        } = self;
        // The curve is written into the params' own (kept across frames): no allocation.
        let curve = params.gamma.get_or_insert_with(Pwl::default);
        if t.ce_enable
            && (t.lo_max != 0.0 || t.hi_max != 0.0)
            && stats.histogram.total() > 0
            && stretch_into(&stats.histogram, t, points, stretch)
        {
            stretch.compose_into(&t.gamma_curve, curve, scratch);
        } else {
            curve.clone_from(&t.gamma_curve);
        }
        let (b, c) = (meta.controls.brightness, meta.controls.contrast);
        if b != 0.0 || c != 1.0 {
            curve.map_y_in_place(|_, y| ((y - 0.5) * c + 0.5 + b).clamp(0.0, 1.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_pulls_empty_ends() {
        // Everything between 0.25 and 0.75.
        let mut bins = vec![0u64; 64];
        bins[16..48].iter_mut().for_each(|b| *b = 10);
        let h = Histogram::from(bins);
        let t = ContrastTuning {
            lo_max: 0.5,
            hi_max: 0.5,
            ..Default::default()
        };
        let s = stretch_curve(&h, &t).unwrap();
        assert!(s.eval(0.26) < 0.05, "{s:?}");
        assert!((s.eval(0.5) - 0.5).abs() < 0.02);
        assert!(s.eval(0.73) > 0.9);
    }

    #[test]
    fn default_gamma_is_monotonic_srgb() {
        let g = default_gamma();
        assert!(g.points().windows(2).all(|w| w[1].1 > w[0].1));
        assert!((g.eval(0.18) - 0.461).abs() < 0.01);
        assert_eq!(g.domain(), (0.0, 1.0));
    }
}
