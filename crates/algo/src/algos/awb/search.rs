//! The AWB estimators: grey world and the Bayesian colour-temperature search.
//!
//! Ported from Raspberry Pi's `awb.cpp` and `awb_bayes.cpp` (BSD-2-Clause, Copyright (C)
//! 2019-2025 Raspberry Pi Ltd). Zone values here are ratios (R/G, B/G) of normalised means, so
//! the `+ 1` guards against 16-bit integer zero divisions in the original are replaced by an
//! explicit check on G.

use alloc::vec::Vec;

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

use crate::pwl::Pwl;

use super::tuning::{AwbMode, AwbPrior, AwbTuning, CtCurve};

/// A usable zone: mean (r, g, b).
pub(crate) type Zone = (f64, f64, f64);

/// An estimate: temperature (K), red gain, blue gain.
pub(crate) type Estimate = (f64, f64, f64);

/// Grey world: average the middle half of the zones sorted by R/G (and B/G).
pub(crate) fn grey_world(zones: &[Zone], default_ct: f64) -> Estimate {
    let mut by_r = zones.to_vec();
    let mut by_b = zones.to_vec();
    // By R/G and B/G; zones without green last (cross-multiplied comparisons are no order
    // with them: every such zone compares equal to every other, which panicked the sort in a
    // dark scene on the device).
    let ratio = |c: f64, g: f64| if g > 0.0 { c / g } else { f64::INFINITY };
    by_r.sort_by(|a, b| ratio(a.0, a.1).total_cmp(&ratio(b.0, b.1)));
    by_b.sort_by(|a, b| ratio(a.2, a.1).total_cmp(&ratio(b.2, b.1)));
    let discard = zones.len() / 4;
    let keep = zones.len() - 2 * discard;
    let (mut rr, mut rg, mut bb, mut bg) = (0.0, 0.0, 0.0, 0.0);
    for (zr, zb) in by_r.iter().zip(&by_b).skip(discard).take(keep) {
        rr += zr.0;
        rg += zr.1;
        bb += zb.2;
        bg += zb.1;
    }
    let gain = |g: f64, c: f64| if c > 0.0 { g / c } else { 1.0 };
    (default_ct, gain(rg, rr), gain(bg, bb))
}

/// The prior for a lux level, interpolated between the tuned levels.
pub(crate) fn interpolate_prior(priors: &[AwbPrior], lux: f64) -> Pwl {
    let (first, last) = (&priors[0], &priors[priors.len() - 1]);
    if lux <= first.lux {
        return first.prior.clone();
    }
    if lux >= last.lux {
        return last.prior.clone();
    }
    let i = priors.windows(2).position(|w| w[1].lux >= lux).unwrap_or(0);
    let (p0, p1) = (&priors[i], &priors[i + 1]);
    let f = (lux - p0.lux) / (p1.lux - p0.lux);
    Pwl::combine(&p0.prior, &p1.prior, |_, y0, y1| y0 + (y1 - y0) * f)
}

/// The Bayesian search over zones already reduced to (R/G, B/G).
pub(crate) struct Search<'a> {
    pub tuning: &'a AwbTuning,
    pub curve: &'a CtCurve,
    pub zones: &'a [(f64, f64)],
    pub prior: Pwl,
    /// The temperature of the last estimate's best minimum (mired), which the hysteresis
    /// prefers.
    pub anchor: Option<f64>,
}

impl Search<'_> {
    /// Sum of capped squared colour errors (non-greyness) for gains.
    fn delta2_sum(&self, gain_r: f64, gain_b: f64) -> f64 {
        let t = self.tuning;
        self.zones
            .iter()
            .map(|&(r, b)| {
                let dr = gain_r * r - 1.0 - t.whitepoint_r;
                let db = gain_b * b - 1.0 - t.whitepoint_b;
                (dr * dr + db * db).min(t.delta_limit)
            })
            .sum()
    }

    /// The prior log likelihood at `t` minus the hysteresis cost of leaving the anchor.
    fn log_prior(&self, t: f64) -> f64 {
        let tu = self.tuning;
        let well = match self.anchor {
            Some(a) if tu.hysteresis > 0.0 => {
                let d = (1e6 / t - a) / tu.hysteresis_mired;
                tu.hysteresis * (1.0 - (-0.5 * d * d).exp())
            }
            _ => 0.0,
        };
        self.prior.eval_clamped(t) - well
    }

    fn cost(&self, t: f64, r: f64, b: f64) -> f64 {
        self.delta2_sum(1.0 / r, 1.0 / b) - self.log_prior(t)
    }

    /// The whole search: coarse along the curve from `mode.lo` to `mode.hi`, then fine (along
    /// and across the curve) around the coarse result. Returns (t, r, b) and the temperature
    /// of the best coarse point (where the hysteresis holds on).
    ///
    /// With [`AwbTuning::softness`] above 0 the coarse result is the mean (in mired) of the
    /// coarse points weighted by `exp(-(cost - best) / softness)` rather than the best point:
    /// when two separate temperatures fit almost equally well the result moves between them
    /// continuously instead of jumping. A clear minimum still dominates. The fine search
    /// weights its steps along the curve the same way. Softness 0 is Raspberry Pi's search:
    /// a parabola through the best coarse point and its neighbours, and the best fine step
    /// (refined here by a parabola too).
    pub fn run(&self, mode: AwbMode) -> ((f64, f64, f64), f64) {
        let mut points = Vec::new();
        let mut t = mode.lo;
        loop {
            points.push((t, self.cost(t, self.curve.r.eval(t), self.curve.b.eval(t))));
            if t >= mode.hi {
                break;
            }
            t = (t + t / 10.0 * self.tuning.coarse_step).min(mode.hi);
        }
        let best = (0..points.len())
            .min_by(|&a, &b| points[a].1.total_cmp(&points[b].1))
            .expect("at least one point");
        let tau = self.tuning.softness;
        let t = if tau > 0.0 {
            let m = points[best].1;
            let (sw, sm) = points.iter().fold((0.0, 0.0), |(sw, sm), &(t, c)| {
                let w = (-(c - m) / tau).exp();
                (sw + w, sm + w * 1e6 / t)
            });
            1e6 / (sm / sw)
        } else if points.len() > 2 {
            let bp = best.clamp(1, points.len() - 2);
            interpolate_quadratic(points[bp - 1], points[bp], points[bp + 1])
        } else {
            points[best].0
        };
        (self.fine(t), points[best].0)
    }

    /// Search around `t`, along and across the curve; returns (t, r, b), r and b being the
    /// grey's R/G and B/G.
    fn fine(&self, t: f64) -> (f64, f64, f64) {
        let tu = self.tuning;
        let (cr, cb) = (&self.curve.r, &self.curve.b);
        let step = t / 10.0 * tu.coarse_step * 0.1;
        let mut nsteps: i32 = 5;
        let r_diff = cr.eval(t + f64::from(nsteps) * step) - cr.eval(t - f64::from(nsteps) * step);
        let b_diff = cb.eval(t + f64::from(nsteps) * step) - cb.eval(t - f64::from(nsteps) * step);
        let len2 = b_diff * b_diff + r_diff * r_diff;
        if len2 < 1e-6 {
            return (t, cr.eval(t), cb.eval(t));
        }
        // Unit vector orthogonal to the b-versus-r curve.
        let len = len2.sqrt();
        let (tr, tb) = (b_diff / len, -r_diff / len);
        let range = tu.transverse_neg + tu.transverse_pos;
        let num = ((range * 100.0 + 0.5).floor() as i32 + 1).clamp(3, 12);
        nsteps += num;
        // Per step along the curve: the best offset across it and its cost.
        let mut along: Vec<(f64, f64)> = Vec::with_capacity(2 * nsteps as usize + 1);
        for i in -nsteps..=nsteps {
            let tt = t + f64::from(i) * step;
            let prior = self.log_prior(tt);
            let (rc, bc) = (cr.eval(tt), cb.eval(tt));
            let mut pts = Vec::with_capacity(num as usize);
            let mut bp = 0;
            for j in 0..num {
                let off = -tu.transverse_neg + range * f64::from(j) / f64::from(num - 1);
                let (rt, bt) = (rc + tr * off, bc + tb * off);
                let c = self.delta2_sum(1.0 / rt, 1.0 / bt) - prior;
                pts.push((off, c));
                if c < pts[bp].1 {
                    bp = j as usize;
                }
            }
            let bp = bp.clamp(1, num as usize - 2);
            let off = interpolate_quadratic(pts[bp - 1], pts[bp], pts[bp + 1]);
            let (rt, bt) = (rc + tr * off, bc + tb * off);
            along.push((off, self.delta2_sum(1.0 / rt, 1.0 / bt) - prior));
        }
        let tau = tu.softness;
        let (pos, off) = if tau > 0.0 {
            // Weighted as the coarse search is: continuous in the statistics.
            let m = along.iter().map(|a| a.1).fold(f64::INFINITY, f64::min);
            let (mut sw, mut sp, mut so) = (0.0, 0.0, 0.0);
            for (i, &(off, c)) in along.iter().enumerate() {
                let w = (-(c - m) / tau).exp();
                (sw, sp, so) = (sw + w, sp + w * i as f64, so + w * off);
            }
            (sp / sw, so / sw)
        } else {
            let k = (0..along.len())
                .min_by(|&a, &b| along[a].1.total_cmp(&along[b].1))
                .expect("steps along the curve");
            let kc = k.clamp(1, along.len() - 2);
            let x = |i: usize| i as f64;
            let pos = interpolate_quadratic(
                (x(kc - 1), along[kc - 1].1),
                (x(kc), along[kc].1),
                (x(kc + 1), along[kc + 1].1),
            );
            let (i0, f) = (pos.floor().min(x(along.len() - 2)), pos - pos.floor());
            let i0 = i0 as usize;
            (pos, along[i0].0 + (along[i0 + 1].0 - along[i0].0) * f)
        };
        let tt = t + (pos - f64::from(nsteps)) * step;
        let (rt, bt) = (cr.eval(tt) + tr * off, cb.eval(tt) + tb * off);
        (tt, rt, bt)
    }
}

/// The x of the extremum of the parabola through three points, kept within `[a.x, c.x]`.
pub(crate) fn interpolate_quadratic(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    const EPS: f64 = 1e-3;
    let (ca, ba) = ((c.0 - a.0, c.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let den = 2.0 * (ba.1 * ca.0 - ca.1 * ba.0);
    if den.abs() > EPS {
        let num = ba.1 * ca.0 * ca.0 - ca.1 * ba.0 * ba.0;
        return (num / den + a.0).clamp(a.0, c.0);
    }
    if a.1 < c.1 - EPS {
        a.0
    } else if c.1 < a.1 - EPS {
        c.0
    } else {
        b.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quadratic_finds_the_vertex() {
        let f = |x: f64| (x - 2.5) * (x - 2.5);
        let x = interpolate_quadratic((1.0, f(1.0)), (2.0, f(2.0)), (4.0, f(4.0)));
        assert!((x - 2.5).abs() < 1e-9);
    }

    #[test]
    fn grey_world_discards_outliers() {
        let mut zones = vec![(0.5, 1.0, 0.25); 8];
        zones.push((5.0, 1.0, 0.25));
        zones.push((0.01, 1.0, 5.0));
        let (_, r, b) = grey_world(&zones, 4500.0);
        assert!((r - 2.0).abs() < 1e-9, "{r}");
        assert!((b - 4.0).abs() < 1e-9, "{b}");
        // Zones without green (a dark scene) are ordered too.
        let mut dark: Vec<Zone> = (0..40)
            .map(|i| (f64::from(i % 3) * 0.01, f64::from(i % 2) * 0.01, 0.01))
            .collect();
        dark.extend(vec![(0.0, 0.0, 0.0); 40]);
        let (_, r, b) = grey_world(&dark, 4500.0);
        assert!(r.is_finite() && b.is_finite());
    }

    #[test]
    fn prior_interpolates_by_lux() {
        let priors = vec![
            AwbPrior {
                lux: 0.0,
                prior: Pwl::from_flat(&[2000.0, 1.0, 8000.0, 1.0]).unwrap(),
            },
            AwbPrior {
                lux: 100.0,
                prior: Pwl::from_flat(&[2000.0, 3.0, 8000.0, 3.0]).unwrap(),
            },
        ];
        assert_eq!(interpolate_prior(&priors, 50.0).eval(4000.0), 2.0);
        assert_eq!(interpolate_prior(&priors, 500.0).eval(4000.0), 3.0);
    }
}
