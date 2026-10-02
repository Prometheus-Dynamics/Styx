//! AWB: automatic white balance.
//!
//! Ported from Raspberry Pi's `awb.cpp` / `awb_bayes.cpp` (BSD-2-Clause, Copyright (C)
//! 2019-2025 Raspberry Pi Ltd). The original runs the search on a thread and picks the result
//! up a frame or more later; here it runs synchronously every `frame_period` frames (every
//! frame during start-up), which keeps the algorithm deterministic. Between estimates the
//! gains move towards the estimate by `speed` per frame.
//!
//! Styx changes: start-up (an estimate every frame, applied at once) counts only frames whose
//! exposure is usable (mean luma between [`USABLE_Y`]'s bounds), so a camera that starts in
//! the dark or saturated still gets `startup_frames` good estimates before the slow filter
//! takes over (and leaves start-up after four times as many frames whatever they were). A warm
//! start ([`crate::WarmStart`]) starts from the last session's gains.
//!
//! Estimators: the Bayesian search when the tuning has a CT curve, priors and modes (steps along
//! the CT curve minimising the zones' colour error minus the prior log likelihood for the
//! current lux, then searches across the curve); grey world otherwise.

mod search;
pub mod tuning;

use crate::config::CameraConfig;
use crate::error::Result;
use crate::frame::FrameMetadata;
use crate::params::{AwbStatus, Params};
use crate::pipeline::Algorithm;
use crate::stats::Statistics;
use crate::warm::WarmStart;

use search::{Estimate, Search, Zone};
use tuning::{AwbTuning, CtCurve};

/// Colour temperature reported by grey world, which cannot estimate one.
const DEFAULT_CT: f64 = 4500.0;

/// Mean luma range of a frame whose colours count as a start-up estimate: neither lost in the
/// noise floor nor mostly clipped.
pub const USABLE_Y: (f64, f64) = (0.02, 0.7);

/// The AWB algorithm. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Awb {
    tuning: AwbTuning,
    curve: Option<CtCurve>,
    bayes: bool,
    frame_count: u32,
    frames_seen: u32,
    unsettled_frames: u32,
    frame_phase: u32,
    estimate: Estimate,
    filtered: Estimate,
}

impl Awb {
    /// AWB with a tuning.
    pub fn new(tuning: AwbTuning) -> Result<Self> {
        tuning.validate()?;
        let curve = tuning.curve()?;
        let bayes = tuning.uses_bayes();
        let mut awb = Self {
            tuning,
            curve,
            bayes,
            frame_count: 0,
            frames_seen: 0,
            unsettled_frames: 0,
            frame_phase: 0,
            estimate: (DEFAULT_CT, 1.0, 1.0),
            filtered: (DEFAULT_CT, 1.0, 1.0),
        };
        awb.reset();
        Ok(awb)
    }

    /// Uses the Bayesian search (else grey world).
    pub fn is_bayes(&self) -> bool {
        self.bayes
    }

    fn reset(&mut self) {
        self.frame_count = 0;
        self.frames_seen = 0;
        self.frame_phase = 0;
        self.estimate = match (&self.curve, self.bayes) {
            (Some(c), true) => self.gains_for_ct(c, 4000.0),
            _ => (DEFAULT_CT, 1.0, 1.0),
        };
        self.filtered = self.estimate;
    }

    fn gains_for_ct(&self, c: &CtCurve, ct: f64) -> Estimate {
        let ct = ct.clamp(c.r.domain().0, c.r.domain().1);
        (ct, 1.0 / c.r.eval(ct), 1.0 / c.b.eval(ct))
    }

    /// Usable zones: enough pixels, enough green, biased and shading-compensated as tuned.
    fn zones(&self, stats: &Statistics, params: &Params) -> Vec<Zone> {
        let t = &self.tuning;
        let bias = match (&self.curve, self.bayes && t.bias_proportion > 0.0) {
            (Some(c), true) => Some((c.r.eval(t.bias_ct), c.b.eval(t.bias_ct))),
            _ => None,
        };
        let lsc = params.lens_shading.as_ref().filter(|l| {
            stats.before_lsc
                && (l.width, l.height) == (stats.colour.width, stats.colour.height)
                && l.r.len() == stats.colour.len()
        });
        let mut out = Vec::new();
        for (i, z) in stats.colour.zones.iter().enumerate() {
            if f64::from(z.counted) < t.min_pixels || z.counted == 0 {
                continue;
            }
            let (mut r, mut g, mut b) = z.mean();
            if g < t.min_g {
                continue;
            }
            if let Some((br, bb)) = bias {
                // Mix in grey pixels of the bias temperature at this zone's green level.
                let p = t.bias_proportion;
                (r, b, g) = (r + p * g * br, b + p * g * bb, g * (1.0 + p));
            }
            if let Some(l) = lsc {
                (r, g, b) = (r * l.r[i], g * l.g[i], b * l.b[i]);
            }
            out.push((r * t.sensitivity_r, g, b * t.sensitivity_b));
        }
        out
    }

    fn estimate(
        &self,
        stats: &Statistics,
        meta: &FrameMetadata,
        params: &Params,
    ) -> Option<Estimate> {
        let zones = self.zones(stats, params);
        if zones.len() <= self.tuning.min_regions as usize {
            return None;
        }
        let t = &self.tuning;
        let curve = match (&self.curve, self.bayes) {
            (Some(c), true) => c,
            _ => return Some(search::grey_world(&zones, DEFAULT_CT)),
        };
        let ratios: Vec<(f64, f64)> = zones
            .iter()
            .filter(|z| z.1 > 0.0)
            .map(|z| (z.0 / z.1, z.2 / z.1))
            .collect();
        let lux = meta.lux.unwrap_or(params.lux);
        let scale = ratios.len() as f64 / stats.colour.len().max(1) as f64;
        let prior = search::interpolate_prior(&t.priors, lux).scale_y(scale);
        let mode = meta
            .controls
            .awb_mode
            .as_ref()
            .and_then(|m| t.modes.get(m))
            .unwrap_or(&t.modes[&t.default_mode]);
        let s = Search {
            tuning: t,
            curve,
            zones: &ratios,
            prior,
        };
        let ct = s.coarse(*mode);
        let (ct, r, b) = s.fine(ct);
        Some((ct, t.sensitivity_r / r, t.sensitivity_b / b))
    }

    /// Manual gains or temperature, if the controls ask for them.
    fn manual(&self, meta: &FrameMetadata) -> Option<Estimate> {
        let c = &meta.controls;
        if let Some((r, b)) = c.colour_gains.filter(|_| !c.awb_enable) {
            let ct = match &self.curve {
                Some(cv) if self.bayes => match (&cv.r_inv, &cv.b_inv) {
                    (Some(ri), Some(bi)) => {
                        (ri.eval_clamped(1.0 / r) + bi.eval_clamped(1.0 / b)) / 2.0
                    }
                    _ => self.filtered.0,
                },
                _ => self.filtered.0,
            };
            return Some((ct, r, b));
        }
        if let (Some(ct), Some(cv), false) = (c.colour_temperature, &self.curve, c.awb_enable) {
            return Some(self.gains_for_ct(cv, ct));
        }
        None
    }
}

impl Algorithm for Awb {
    fn name(&self) -> &'static str {
        "awb"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        self.unsettled_frames = config.unsettled_frames;
        self.reset();
        Ok(())
    }

    fn warm_start(&mut self, warm: &WarmStart) {
        if !warm.is_valid() {
            return;
        }
        let g = warm.colour_gains;
        let e = (warm.colour_temperature, g[0] / g[1], g[2] / g[1]);
        self.estimate = e;
        self.filtered = e;
    }

    fn initial(&self, params: &mut Params) {
        params.colour_gains = [self.filtered.1, 1.0, self.filtered.2];
        params.colour_temperature = self.filtered.0;
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        let mode = meta
            .controls
            .awb_mode
            .clone()
            .filter(|m| self.tuning.modes.contains_key(m))
            .unwrap_or_else(|| self.tuning.default_mode.clone());
        let auto;
        if let Some(m) = self.manual(meta) {
            // Manual values apply at once.
            (self.estimate, self.filtered, auto) = (m, m, false);
        } else if !meta.controls.awb_enable {
            auto = false;
            self.estimate = self.filtered;
        } else if meta.frame < u64::from(self.unsettled_frames) {
            // Levels not settled yet (see `CameraConfig::unsettled_frames`): keep the gains.
            auto = true;
        } else {
            auto = true;
            let y = stats.mean_luma();
            self.frames_seen = self.frames_seen.saturating_add(1);
            if self.frame_count < self.tuning.startup_frames
                && (USABLE_Y.0..=USABLE_Y.1).contains(&y)
            {
                self.frame_count += 1;
            }
            let startup = self.frame_count < self.tuning.startup_frames
                && self.frames_seen < self.tuning.startup_frames.saturating_mul(4);
            self.frame_phase = self.frame_phase.saturating_add(1);
            if startup || self.frame_phase >= self.tuning.frame_period {
                if let Some(e) = self.estimate(stats, meta, params) {
                    self.estimate = e;
                }
                self.frame_phase = 0;
            }
            let speed = if startup { 1.0 } else { self.tuning.speed };
            let mix = |a: f64, b: f64| speed * a + (1.0 - speed) * b;
            self.filtered = (
                mix(self.estimate.0, self.filtered.0),
                mix(self.estimate.1, self.filtered.1),
                mix(self.estimate.2, self.filtered.2),
            );
        }
        let (e, f) = (self.estimate, self.filtered);
        let close = |a: f64, b: f64| (a - b).abs() <= 0.01 * b.abs();
        params.colour_gains = [f.1, 1.0, f.2];
        params.colour_temperature = f.0;
        params.awb = AwbStatus {
            auto,
            mode,
            estimate: e,
            converged: close(f.1, e.1) && close(f.2, e.2),
        };
    }
}

#[cfg(test)]
mod tests;
