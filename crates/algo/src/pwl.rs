//! Piecewise-linear functions, the curve type used throughout tuning data (targets by lux,
//! priors by colour temperature, gamma curves).
//!
//! Semantics follow libcamera's `ipa::Pwl` (BSD-2-Clause, Copyright Raspberry Pi Ltd and Ideas
//! on Board Oy): evaluation extrapolates the end segments, a single point is a constant, and
//! callers clamp to the [`Pwl::domain`] where extrapolation is unwanted. Written from those
//! semantics, not translated.

use serde::{Deserialize, Serialize};

use crate::error::{AlgoError, Result};

/// A piecewise-linear function through points with strictly increasing x.
///
/// Serialised as a flat list `[x0, y0, x1, y1, ...]` (the Raspberry Pi tuning form); a single
/// number deserialises as a constant.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(try_from = "PwlRepr", into = "Vec<f64>")]
pub struct Pwl {
    points: Vec<(f64, f64)>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PwlRepr {
    Constant(f64),
    Flat(Vec<f64>),
}

impl TryFrom<PwlRepr> for Pwl {
    type Error = AlgoError;
    fn try_from(r: PwlRepr) -> Result<Self> {
        match r {
            PwlRepr::Constant(y) => Pwl::new(vec![(0.0, y)]),
            PwlRepr::Flat(v) => Pwl::from_flat(&v),
        }
    }
}

impl From<Pwl> for Vec<f64> {
    fn from(p: Pwl) -> Self {
        p.points.iter().flat_map(|&(x, y)| [x, y]).collect()
    }
}

const EPS: f64 = 1e-6;

impl Pwl {
    /// A function through `points` (x strictly increasing, all finite, at least one point).
    pub fn new(points: Vec<(f64, f64)>) -> Result<Self> {
        if points.is_empty() {
            return Err(AlgoError::tuning("curve has no points"));
        }
        if points.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
            return Err(AlgoError::tuning("curve has a non-finite value"));
        }
        if points.windows(2).any(|w| w[1].0 <= w[0].0) {
            return Err(AlgoError::tuning("curve x values must increase strictly"));
        }
        Ok(Self { points })
    }

    /// From `[x0, y0, x1, y1, ...]`.
    pub fn from_flat(v: &[f64]) -> Result<Self> {
        if v.is_empty() || !v.len().is_multiple_of(2) {
            return Err(AlgoError::tuning(format!(
                "curve needs x, y pairs, got {} values",
                v.len()
            )));
        }
        Self::new(v.chunks(2).map(|c| (c[0], c[1])).collect())
    }

    /// A constant.
    pub fn constant(y: f64) -> Self {
        Self {
            points: vec![(0.0, y)],
        }
    }

    /// The points.
    pub fn points(&self) -> &[(f64, f64)] {
        &self.points
    }

    /// No points (only possible for `Default`).
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// First and last x.
    pub fn domain(&self) -> (f64, f64) {
        match (self.points.first(), self.points.last()) {
            (Some(a), Some(b)) => (a.0, b.0),
            _ => (0.0, 0.0),
        }
    }

    /// Smallest and largest y.
    pub fn range(&self) -> (f64, f64) {
        self.points
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(p.1), hi.max(p.1))
            })
    }

    /// Index of the segment used for `x` (end segments extend outwards).
    fn span(&self, x: f64) -> usize {
        let n = self.points.len();
        if n < 2 {
            return 0;
        }
        let i = self.points.partition_point(|p| p.0 <= x);
        i.saturating_sub(1).min(n - 2)
    }

    /// Value at `x`, extrapolating the end segments. An empty curve is 0.
    pub fn eval(&self, x: f64) -> f64 {
        match self.points.len() {
            0 => 0.0,
            1 => self.points[0].1,
            _ => {
                let i = self.span(x);
                let (x0, y0) = self.points[i];
                let (x1, y1) = self.points[i + 1];
                y0 + (x - x0) * (y1 - y0) / (x1 - x0)
            }
        }
    }

    /// Value at `x` clamped into the domain (no extrapolation).
    pub fn eval_clamped(&self, x: f64) -> f64 {
        let (lo, hi) = self.domain();
        self.eval(x.clamp(lo, hi))
    }

    /// The inverse function, when y is strictly monotonic.
    pub fn inverse(&self) -> Option<Pwl> {
        let inc = self.points.windows(2).all(|w| w[1].1 > w[0].1);
        let dec = self.points.windows(2).all(|w| w[1].1 < w[0].1);
        if !(inc || dec) {
            return None;
        }
        let mut pts: Vec<(f64, f64)> = self.points.iter().map(|&(x, y)| (y, x)).collect();
        if dec {
            pts.reverse();
        }
        Pwl::new(pts).ok()
    }

    /// `other(self(x))`, exact for piecewise-linear inputs: breakpoints are this curve's points
    /// plus the x where this curve crosses one of `other`'s breakpoints.
    pub fn compose(&self, other: &Pwl) -> Pwl {
        let mut xs: Vec<f64> = self.points.iter().map(|p| p.0).collect();
        for w in self.points.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            if (y1 - y0).abs() <= EPS {
                continue;
            }
            for &(ox, _) in &other.points {
                let (lo, hi) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
                if ox > lo && ox < hi {
                    xs.push(x0 + (ox - y0) * (x1 - x0) / (y1 - y0));
                }
            }
        }
        Self::from_xs(xs, |x| other.eval(self.eval(x)))
    }

    /// `f(x, a(x), b(x))` over the union of both curves' breakpoints.
    pub fn combine(a: &Pwl, b: &Pwl, f: impl Fn(f64, f64, f64) -> f64) -> Pwl {
        let xs = a.points.iter().chain(&b.points).map(|p| p.0).collect();
        Self::from_xs(xs, |x| f(x, a.eval(x), b.eval(x)))
    }

    fn from_xs(mut xs: Vec<f64>, f: impl Fn(f64) -> f64) -> Pwl {
        xs.sort_by(f64::total_cmp);
        let mut points: Vec<(f64, f64)> = Vec::with_capacity(xs.len());
        for x in xs {
            if points.last().is_none_or(|p| x - p.0 > EPS) {
                points.push((x, f(x)));
            }
        }
        Pwl { points }
    }

    /// Multiply every y by `k`.
    pub fn scale_y(&self, k: f64) -> Pwl {
        self.map_y(|_, y| y * k)
    }

    /// Replace every y by `f(x, y)`.
    pub fn map_y(&self, f: impl Fn(f64, f64) -> f64) -> Pwl {
        Pwl {
            points: self.points.iter().map(|&(x, y)| (x, f(x, y))).collect(),
        }
    }

    /// Replace every x by `f(x)` (must keep x increasing).
    pub fn map_x(&self, f: impl Fn(f64) -> f64) -> Result<Pwl> {
        Pwl::new(self.points.iter().map(|&(x, y)| (f(x), y)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[f64]) -> Pwl {
        Pwl::from_flat(v).unwrap()
    }

    #[test]
    fn eval_interpolates_and_extrapolates() {
        let c = p(&[0.0, 0.0, 10.0, 10.0, 20.0, 0.0]);
        assert_eq!(c.eval(5.0), 5.0);
        assert_eq!(c.eval(15.0), 5.0);
        assert_eq!(c.eval(-1.0), -1.0);
        assert_eq!(c.eval(21.0), -1.0);
        assert_eq!(c.eval_clamped(21.0), 0.0);
        assert_eq!(Pwl::constant(3.0).eval(100.0), 3.0);
    }

    #[test]
    fn rejects_bad_curves() {
        assert!(Pwl::from_flat(&[0.0, 1.0, 0.0, 2.0]).is_err());
        assert!(Pwl::from_flat(&[0.0, 1.0, 2.0]).is_err());
        assert!(Pwl::from_flat(&[]).is_err());
    }

    #[test]
    fn inverse_of_decreasing_curve() {
        let c = p(&[2000.0, 1.0, 6000.0, 0.5]);
        let inv = c.inverse().unwrap();
        assert!((inv.eval(0.75) - 4000.0).abs() < 1e-9);
        assert!(p(&[0.0, 0.0, 1.0, 1.0, 2.0, 0.0]).inverse().is_none());
    }

    #[test]
    fn compose_is_exact() {
        let a = p(&[0.0, 0.0, 1.0, 1.0]);
        let b = p(&[0.0, 0.0, 0.5, 0.8, 1.0, 1.0]);
        let c = a.compose(&b);
        for i in 0..=20 {
            let x = i as f64 / 20.0;
            assert!((c.eval(x) - b.eval(a.eval(x))).abs() < 1e-12);
        }
        assert_eq!(c.points().len(), 3);
    }

    #[test]
    fn combine_interpolates_between_curves() {
        let a = p(&[0.0, 0.0, 10.0, 10.0]);
        let b = p(&[0.0, 10.0, 5.0, 10.0, 10.0, 0.0]);
        let m = Pwl::combine(&a, &b, |_, ya, yb| (ya + yb) / 2.0);
        assert_eq!(m.points().len(), 3);
        assert_eq!(m.eval(5.0), 7.5);
    }

    #[test]
    fn serde_flat_and_constant() {
        let c: Pwl = serde_json::from_str("[0, 1, 2, 3]").unwrap();
        assert_eq!(c.points(), &[(0.0, 1.0), (2.0, 3.0)]);
        let k: Pwl = serde_json::from_str("0.5").unwrap();
        assert_eq!(k.eval(9.0), 0.5);
        assert_eq!(serde_json::to_string(&c).unwrap(), "[0.0,1.0,2.0,3.0]");
    }
}
