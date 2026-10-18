//! The zones' colour error summed over the search's candidate gains.
//!
//! The Bayesian search scores every candidate point of the CT curve (and of the fine search
//! across it) by `Σ_zones min(error², delta_limit)`, the zones' squared colour error at the
//! candidate's gains capped at `delta_limit`. The raw zones are a 32 × 32 grid: about a thousand
//! terms per candidate, a few hundred candidates per frame.
//!
//! [`Binned`] groups the zones into cells of their (R/G, B/G) once per frame. For a candidate,
//! a cell is then one of three things, and each is exact:
//!
//! - entirely under the cap: its zones' errors sum to `n · (error at the mean + gains² · the
//!   zones' variances)`, since the error is quadratic in the ratios;
//! - entirely over the cap: `n · delta_limit`;
//! - straddling it (the error over the cell's bounding box crosses the cap): its zones are
//!   summed one by one.
//!
//! So the sum equals the sum over the zones, up to rounding, and only the straddling cells cost
//! a term per zone.

// The search needs feature `awb-bayes` (on with `std`); the buffers are always there.
#![cfg_attr(not(feature = "awb-bayes"), allow(dead_code))]

use alloc::vec::Vec;

use super::tuning::AwbTuning;

/// The finest cell size, in ratio units (R/G or B/G).
pub(crate) const BIN: f64 = 0.02;

/// Most cells a side of the grid the zones are binned on.
pub(crate) const GRID: usize = 64;

/// The sum of the capped colour errors of the zones at a candidate's gains.
pub(crate) trait ZoneCost {
    /// `Σ_zones min((gain_r · r − 1 − whitepoint_r)² + (gain_b · b − 1 − whitepoint_b)², limit)`.
    fn delta2_sum(&self, tuning: &AwbTuning, gain_r: f64, gain_b: f64) -> f64;
}

/// A cell of the grid: its zones (`members[start..start + n]`), their mean (R/G, B/G), their
/// variances and the box they lie in. While filling, `r`, `b`, `vr` and `vb` hold sums.
#[derive(Debug, Clone, Copy)]
struct Cell {
    n: f64,
    start: usize,
    r: f64,
    b: f64,
    vr: f64,
    vb: f64,
    lo: (f64, f64),
    hi: (f64, f64),
    /// The largest distance of a zone's ratios from the mean, (R/G, B/G).
    dev: (f64, f64),
}

const EMPTY: Cell = Cell {
    n: 0.0,
    start: 0,
    r: 0.0,
    b: 0.0,
    vr: 0.0,
    vb: 0.0,
    lo: (f64::INFINITY, f64::INFINITY),
    hi: (f64::NEG_INFINITY, f64::NEG_INFINITY),
    dev: (0.0, 0.0),
};

/// The zones binned into cells (see the [module documentation](self)). The buffers are kept
/// across frames.
#[derive(Debug, Default, Clone)]
pub(crate) struct Binned {
    cells: Vec<Cell>,
    members: Vec<(f64, f64)>,
    /// Per grid box: its cell's index, or [`NONE`].
    index: Vec<u32>,
    cursor: Vec<usize>,
}

/// No cell in a grid box.
const NONE: u32 = u32::MAX;

impl Binned {
    /// Bins `zones` (R/G, B/G), replacing what was binned before. Zones that are not finite are
    /// left out.
    pub(crate) fn fill(&mut self, zones: &[(f64, f64)]) {
        self.cells.clear();
        self.members.clear();
        let finite = |z: &&(f64, f64)| z.0.is_finite() && z.1.is_finite();
        let (mut lo, mut hi) = (
            (f64::INFINITY, f64::INFINITY),
            (f64::NEG_INFINITY, f64::NEG_INFINITY),
        );
        let mut any = false;
        for &(r, b) in zones.iter().filter(finite) {
            any = true;
            lo = (lo.0.min(r), lo.1.min(b));
            hi = (hi.0.max(r), hi.1.max(b));
        }
        if !any {
            return;
        }
        let span = (hi.0 - lo.0).max(hi.1 - lo.1);
        let step = BIN.max(span / (GRID - 1) as f64);
        let nr = ((hi.0 - lo.0) / step) as usize + 1;
        let nb = ((hi.1 - lo.1) / step) as usize + 1;
        let cell_of = |r: f64, b: f64| {
            ((((r - lo.0) / step) as usize).min(nr - 1)) * nb
                + (((b - lo.1) / step) as usize).min(nb - 1)
        };
        // The grid maps each box to its occupied cell (in `cells`, in order of first zone).
        self.index.clear();
        self.index.resize(nr * nb, NONE);
        for &(r, b) in zones.iter().filter(finite) {
            let g = cell_of(r, b);
            if self.index[g] == NONE {
                self.index[g] = self.cells.len() as u32;
                self.cells.push(EMPTY);
            }
            let c = &mut self.cells[self.index[g] as usize];
            c.n += 1.0;
            c.r += r;
            c.b += b;
            c.vr += r * r;
            c.vb += b * b;
            c.lo = (c.lo.0.min(r), c.lo.1.min(b));
            c.hi = (c.hi.0.max(r), c.hi.1.max(b));
        }
        // Each cell's zones are contiguous in `members`, from its `start`.
        let mut total = 0;
        self.cursor.clear();
        for c in &mut self.cells {
            c.start = total;
            self.cursor.push(total);
            total += c.n as usize;
        }
        self.members.resize(total, (0.0, 0.0));
        for &(r, b) in zones.iter().filter(finite) {
            let i = self.index[cell_of(r, b)] as usize;
            self.members[self.cursor[i]] = (r, b);
            self.cursor[i] += 1;
        }
        for c in &mut self.cells {
            let (r, b) = (c.r / c.n, c.b / c.n);
            (c.r, c.b) = (r, b);
            c.vr = (c.vr / c.n - r * r).max(0.0);
            c.vb = (c.vb / c.n - b * b).max(0.0);
            c.dev = ((c.hi.0 - r).max(r - c.lo.0), (c.hi.1 - b).max(b - c.lo.1));
        }
    }

    /// Occupied cells.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.cells.len()
    }
}

impl ZoneCost for Binned {
    fn delta2_sum(&self, t: &AwbTuning, gain_r: f64, gain_b: f64) -> f64 {
        let (a, c) = (1.0 + t.whitepoint_r, 1.0 + t.whitepoint_b);
        let limit = t.delta_limit;
        self.cells
            .iter()
            .map(|cell| {
                // The cell's errors lie between these bounds: the error at the mean, moved by
                // the gains times the largest deviation of a zone.
                let (x, y) = (gain_r * cell.r - a, gain_b * cell.b - c);
                let (dx, dy) = (gain_r * cell.dev.0, gain_b * cell.dev.1);
                let (x, y) = (x.abs(), y.abs());
                let far = (x + dx) * (x + dx) + (y + dy) * (y + dy);
                if far <= limit {
                    // Every zone under the cap: their errors' sum, from the mean.
                    let mean =
                        x * x + y * y + gain_r * gain_r * cell.vr + gain_b * gain_b * cell.vb;
                    cell.n * mean
                } else {
                    let (nx, ny) = ((x - dx).max(0.0), (y - dy).max(0.0));
                    if nx * nx + ny * ny >= limit {
                        cell.n * limit
                    } else {
                        let end = cell.start + cell.n as usize;
                        self.members[cell.start..end]
                            .iter()
                            .map(|&(r, b)| {
                                let dr = gain_r * r - a;
                                let db = gain_b * b - c;
                                (dr * dr + db * db).min(limit)
                            })
                            .sum()
                    }
                }
            })
            .sum()
    }
}

/// The zones themselves, one term each: the sum as it was before binning (the reference the
/// tests compare [`Binned`] with).
#[cfg(test)]
pub(crate) struct Scalar<'a>(pub &'a [(f64, f64)]);

#[cfg(test)]
impl ZoneCost for Scalar<'_> {
    fn delta2_sum(&self, t: &AwbTuning, gain_r: f64, gain_b: f64) -> f64 {
        self.0
            .iter()
            .map(|&(r, b)| {
                let dr = gain_r * r - 1.0 - t.whitepoint_r;
                let db = gain_b * b - 1.0 - t.whitepoint_b;
                (dr * dr + db * db).min(t.delta_limit)
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::super::search::{Search, SearchBuffers};
    use super::super::tuning::{AwbMode, CtCurve};
    use super::*;
    use crate::pwl::Pwl;
    use std::time::Instant;

    struct Rng(u64);

    impl Rng {
        /// Uniform in `[0, 1)`.
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }

        fn range(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * self.next()
        }

        fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[(self.next() * xs.len() as f64) as usize % xs.len()]
        }
    }

    fn curve_tuning() -> AwbTuning {
        AwbTuning {
            ct_curve: vec![[2500.0, 1.0, 0.4], [4500.0, 0.7, 0.6], [8000.0, 0.45, 0.85]],
            ..Default::default()
        }
    }

    /// `n` zones under an illuminant at a random CT: grey with noise, some coloured patches and
    /// some dark, noisy ones.
    fn grid_zones(rng: &mut Rng, curve: &CtCurve, n: usize) -> Vec<(f64, f64)> {
        let ct = rng.range(2600.0, 7800.0);
        let (r, b) = (curve.r.eval(ct), curve.b.eval(ct));
        (0..n)
            .map(|_| {
                let u = rng.next();
                let (noise, patch) = if u < 0.05 {
                    (0.25, rng.range(0.5, 2.0))
                } else if u < 0.2 {
                    (0.02, rng.range(0.6, 1.6))
                } else {
                    (0.02, 1.0)
                };
                let jitter = |rng: &mut Rng| 1.0 + noise * (rng.next() - 0.5);
                let (zr, zb) = (r * patch * jitter(rng), b * jitter(rng) / patch.sqrt());
                (zr.max(1e-3), zb.max(1e-3))
            })
            .collect()
    }

    /// The search over the raw zones and over the binned ones, on random tuning, modes,
    /// anchors and zones: the CT and the gains must agree (the binned sum is exact up to
    /// rounding, so the search only differs by that).
    #[test]
    fn binned_search_matches_the_zones() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let (mut worst_ct, mut worst_gain, mut worst_best) = (0.0f64, 0.0f64, 0.0f64);
        for case in 0..600 {
            let mut tu = curve_tuning();
            tu.coarse_step = rng.pick(&[0.1, 0.2, 0.3]);
            tu.softness = rng.pick(&[0.0, 0.2, 0.5]);
            tu.delta_limit = rng.pick(&[0.05, 0.2, 0.5]);
            tu.transverse_pos = rng.range(0.005, 0.04);
            tu.transverse_neg = rng.range(0.005, 0.04);
            tu.whitepoint_r = rng.range(-0.02, 0.02);
            tu.whitepoint_b = rng.range(-0.02, 0.02);
            let curve = tu.curve().unwrap().unwrap();
            let prior = Pwl::from_flat(&[2500.0, rng.range(0.0, 2.0), 8000.0, rng.range(0.0, 2.0)])
                .unwrap();
            let mode = AwbMode {
                lo: rng.range(2500.0, 4000.0),
                hi: rng.range(5000.0, 8000.0),
            };
            let anchor = (rng.next() < 0.5).then(|| 1e6 / rng.range(3000.0, 7000.0));
            let n = if case % 4 == 0 {
                16 + (rng.next() * 500.0) as usize
            } else {
                1024
            };
            let zones = grid_zones(&mut rng, &curve, n);
            let mut bins = Binned::default();
            bins.fill(&zones);
            let scalar = Scalar(&zones);
            let old = Search {
                tuning: &tu,
                curve: &curve,
                zones: &scalar,
                prior: &prior,
                anchor,
            };
            let new = Search {
                tuning: &tu,
                curve: &curve,
                zones: &bins,
                prior: &prior,
                anchor,
            };
            let ((ct0, r0, b0), best0) = old.run(mode, &mut SearchBuffers::default());
            let ((ct1, r1, b1), best1) = new.run(mode, &mut SearchBuffers::default());
            worst_ct = worst_ct.max((ct0 - ct1).abs());
            worst_best = worst_best.max((best0 - best1).abs());
            worst_gain = worst_gain
                .max((r0 - r1).abs() / r0)
                .max((b0 - b1).abs() / b0);
            assert!((ct0 - ct1).abs() < 25.0, "case {case}: ct {ct0} vs {ct1}");
            assert!(
                (r0 - r1).abs() / r0 < 0.005 && (b0 - b1).abs() / b0 < 0.005,
                "case {case}: gains {r0} {b0} vs {r1} {b1}"
            );
        }
        println!(
            "COMPARE awb cases=600 max_ct_k={worst_ct:.3e} max_gain_rel={worst_gain:.3e} \
             max_best_k={worst_best:.3e}"
        );
    }

    /// The cell sums against the zones' sums at many gains: the binned sum is exact up to
    /// rounding.
    #[test]
    fn binned_sum_is_the_zones_sum() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let curve = curve_tuning().curve().unwrap().unwrap();
        let mut worst = 0.0f64;
        for _ in 0..200 {
            let mut t = curve_tuning();
            t.delta_limit = rng.pick(&[0.02, 0.05, 0.2, 0.5, 2.0]);
            t.whitepoint_r = rng.range(-0.02, 0.02);
            t.whitepoint_b = rng.range(-0.02, 0.02);
            let zones = grid_zones(&mut rng, &curve, 1024);
            let mut bins = Binned::default();
            bins.fill(&zones);
            for _ in 0..20 {
                let (gr, gb) = (rng.range(0.5, 2.0), rng.range(0.5, 2.0));
                let sum = Scalar(&zones).delta2_sum(&t, gr, gb);
                let binned = bins.delta2_sum(&t, gr, gb);
                worst = worst.max((sum - binned).abs() / sum.max(1e-12));
            }
        }
        println!("COMPARE awb_sum max_rel={worst:.3e}");
        assert!(worst < 1e-9, "{worst}");
    }

    #[test]
    fn identical_zones_are_one_cell() {
        let zones = vec![(1.2, 0.8); 500];
        let mut bins = Binned::default();
        bins.fill(&zones);
        assert_eq!(bins.len(), 1);
        let t = curve_tuning();
        let (gr, gb) = (0.9, 1.1);
        let sum = Scalar(&zones).delta2_sum(&t, gr, gb);
        let binned = bins.delta2_sum(&t, gr, gb);
        assert!(
            (sum - binned).abs() < 1e-9 * sum.max(1.0),
            "{sum} vs {binned}"
        );
    }

    /// The search over 32 × 32 zones: the raw zones (before) and the binned ones with the
    /// binning included (after).
    #[test]
    #[ignore]
    fn bench_awb_search() {
        let tu = curve_tuning();
        let curve = tu.curve().unwrap().unwrap();
        let prior = Pwl::constant(0.0);
        let mode = AwbMode {
            lo: 2500.0,
            hi: 8000.0,
        };
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let zones = grid_zones(&mut rng, &curve, 1024);
        let mut bins = Binned::default();
        let mut bufs = SearchBuffers::default();
        let mut sink = 0.0;
        let iters = 1000;
        let scalar = Scalar(&zones);
        let old = Search {
            tuning: &tu,
            curve: &curve,
            zones: &scalar,
            prior: &prior,
            anchor: None,
        };
        for _ in 0..20 {
            sink += old.run(mode, &mut bufs).0.0;
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            sink += old.run(mode, &mut bufs).0.0;
        }
        let before = t0.elapsed().as_secs_f64() / f64::from(iters) * 1e6;
        let t1 = Instant::now();
        for _ in 0..iters {
            bins.fill(&zones);
            let new = Search {
                tuning: &tu,
                curve: &curve,
                zones: &bins,
                prior: &prior,
                anchor: None,
            };
            sink += new.run(mode, &mut bufs).0.0;
        }
        let after = t1.elapsed().as_secs_f64() / f64::from(iters) * 1e6;
        println!(
            "BENCH awb_search zones=1024 cells={} before_us={before:.2} after_us={after:.2} (sink {sink:.3})",
            bins.len()
        );
    }
}
