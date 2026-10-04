//! The adaptive part of ALSC: refines the red and blue tables from the colour statistics so
//! that neighbouring zones of similar colour come out the same colour.
//!
//! Ported from Raspberry Pi's `alsc.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi Ltd):
//! `calculateCrCb`, `applyCalTable`, `computeW`, `constructM`, `gaussSeidel2Sor` and
//! `runMatrixIterations`. For every zone `i` it solves for gains `λ` such that
//! `λ_i C_i ≈ λ_j C_j` for each neighbour `j` whose colour ratio `C` is close (a weight
//! `exp(-((C_i - C_j) / σ)² / 2)`), by Gauss-Seidel iterations with over-relaxation, each gain
//! bounded to `1 ± lambda_bound`. The original's `reaverage` (and `normalise` of the final
//! tables) leave their input unchanged (their `std::for_each` lambdas return a value instead of
//! assigning it), so this port does not rescale either: the results match what libcamera runs.

use alloc::{vec, vec::Vec};

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

use crate::stats::ZoneGrid;

/// Marks a zone without usable colour statistics.
const INSUFFICIENT: f64 = -1.0;

/// The settings the iterations need (from [`super::AlscTuning`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct Settings {
    pub sigma_cr: f64,
    pub sigma_cb: f64,
    pub min_count: f64,
    pub min_g: f64,
    pub omega: f64,
    pub n_iter: u32,
    pub threshold: f64,
    pub lambda_bound: f64,
}

/// One zone's colour (sums of normalised values and pixels counted), as the iterations see
/// it: already divided by any lens shading applied before the statistics.
pub(super) type Zone = (f64, f64, f64, u32);

/// R/G and B/G per zone, or [`INSUFFICIENT`] where the zone is too dark or too empty. The
/// green and channel limits compare the mean on the 16-bit scale truncated to an integer, as
/// the original does with its integer sums.
fn cr_cb(zones: &[Zone], s: &Settings) -> (Vec<f64>, Vec<f64>) {
    let min_g16 = s.min_g * 65536.0;
    zones
        .iter()
        .map(|&(r, g, b, counted)| {
            let n = f64::from(counted.max(1));
            let low = |v: f64| (v / n * 65536.0).floor() <= min_g16;
            if f64::from(counted) <= s.min_count || low(g) || low(r) || low(b) {
                (INSUFFICIENT, INSUFFICIENT)
            } else {
                (r / g, b / g)
            }
        })
        .unzip()
}

fn weight(ci: f64, cj: f64, sigma: f64) -> f64 {
    if ci == INSUFFICIENT || cj == INSUFFICIENT {
        return 0.0;
    }
    let d = (ci - cj) / sigma;
    (-d * d / 2.0).exp()
}

/// Neighbour weights, `[above, right, below, left]` per zone.
fn weights(c: &[f64], w: usize, sigma: f64) -> Vec<[f64; 4]> {
    let n = c.len();
    (0..n)
        .map(|i| {
            [
                if i >= w {
                    weight(c[i], c[i - w], sigma)
                } else {
                    0.0
                },
                if i % w < w - 1 {
                    weight(c[i], c[i + 1], sigma)
                } else {
                    0.0
                },
                if i < n - w {
                    weight(c[i], c[i + w], sigma)
                } else {
                    0.0
                },
                if i % w != 0 {
                    weight(c[i], c[i - 1], sigma)
                } else {
                    0.0
                },
            ]
        })
        .collect()
}

/// The sparse matrix `M` (`M λ = λ`), diagonal divided out.
fn matrix(c: &[f64], wts: &[[f64; 4]], w: usize) -> Vec<[f64; 4]> {
    const EPSILON: f64 = 0.001;
    let n = c.len();
    (0..n)
        .map(|i| {
            let has = [i >= w, i % w < w - 1, i < n - w, i % w != 0];
            let m = has.iter().filter(|&&h| h).count() as f64;
            let wt = wts[i];
            let diagonal = (EPSILON + wt[0] + wt[1] + wt[2] + wt[3]) * c[i];
            let nb = [i.wrapping_sub(w), i + 1, i + w, i.wrapping_sub(1)];
            let mut row = [0.0; 4];
            for k in 0..4 {
                if has[k] {
                    row[k] = (wt[k] * c[nb[k]] + EPSILON / m * c[i]) / diagonal;
                }
            }
            row
        })
        .collect()
}

/// One zone's new value from its neighbours (zero coefficients where there are none).
fn zone(i: usize, m: &[[f64; 4]], l: &[f64], w: usize) -> f64 {
    let n = l.len();
    let mut v = 0.0;
    if i >= w {
        v += m[i][0] * l[i - w];
    }
    if i + 1 < n {
        v += m[i][1] * l[i + 1];
    }
    if i + w < n {
        v += m[i][2] * l[i + w];
    }
    if i >= 1 {
        v += m[i][3] * l[i - 1];
    }
    v
}

/// A forward and a backward Gauss-Seidel sweep with over-relaxation; returns the largest
/// change (signed).
fn sweep(m: &[[f64; 4]], omega: f64, l: &mut [f64], old: &mut [f64], w: usize, bound: f64) -> f64 {
    let (lo, hi) = (1.0 - bound, 1.0 + bound);
    old.copy_from_slice(l);
    let n = l.len();
    for i in 0..n {
        l[i] = zone(i, m, l, w).clamp(lo, hi);
    }
    // The original then solves from the top down: the last zone again, then all of them.
    for i in (0..n).rev() {
        l[i] = zone(i, m, l, w).clamp(lo, hi);
    }
    let mut max_diff = 0.0f64;
    for (v, o) in l.iter_mut().zip(old.iter()) {
        *v = o + (*v - o) * omega;
        if (*v - o).abs() > max_diff.abs() {
            max_diff = *v - o;
        }
    }
    max_diff
}

fn iterate(c: &[f64], lambda: &mut [f64], w: usize, sigma: f64, s: &Settings) -> u32 {
    let m = matrix(c, &weights(c, w, sigma), w);
    let mut old = vec![0.0; lambda.len()];
    for i in 0..s.n_iter {
        if sweep(&m, s.omega, lambda, &mut old, w, s.lambda_bound).abs() < s.threshold {
            return i + 1;
        }
    }
    s.n_iter
}

/// Runs the iterations for red and blue: `zones` are the statistics, `cal_r` / `cal_b` the
/// calibrated tables for the current temperature (folded into the statistics first, so the
/// iterations only make the extra adjustment), `lambda_r` / `lambda_b` the gains from the last
/// run, updated in place. Returns the number of iterations (red, blue).
pub(super) fn run(
    zones: &[Zone],
    grid: (u32, u32),
    cal: (&[f64], &[f64]),
    lambda: (&mut [f64], &mut [f64]),
    s: &Settings,
) -> (u32, u32) {
    let w = grid.0 as usize;
    let (mut cr, mut cb) = cr_cb(zones, s);
    for (c, t) in [(&mut cr, cal.0), (&mut cb, cal.1)] {
        for (v, k) in c.iter_mut().zip(t) {
            if *v != INSUFFICIENT {
                *v *= k;
            }
        }
    }
    (
        iterate(&cr, lambda.0, w, s.sigma_cr, s),
        iterate(&cb, lambda.1, w, s.sigma_cb, s),
    )
}

/// The statistics as zones for [`run`], dividing out `applied` (r, g, b tables) when the
/// statistics were taken after lens shading; `None` when the grid is not the tables' grid.
pub(super) fn zones_of(
    colour: &ZoneGrid<crate::stats::ColourZone>,
    grid: (u32, u32),
    applied: Option<[&[f64]; 3]>,
) -> Option<Vec<Zone>> {
    if (colour.width, colour.height) != grid || !colour.is_valid() {
        return None;
    }
    Some(
        colour
            .zones
            .iter()
            .enumerate()
            .map(|(i, z)| match applied {
                Some([r, g, b]) => (z.r / r[i], z.g / g[i], z.b / b[i], z.counted),
                None => (z.r, z.g, z.b, z.counted),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            sigma_cr: 0.01,
            sigma_cb: 0.01,
            min_count: 10.0,
            min_g: 50.0 / 65536.0,
            omega: 1.3,
            n_iter: 64,
            threshold: 1e-3,
            lambda_bound: 0.05,
        }
    }

    #[test]
    fn evens_out_a_colour_step_within_a_uniform_surface() {
        // A grey surface whose right half reads 2% redder (a shading residual): the gains
        // bring the halves together.
        let (w, h) = (8u32, 6u32);
        let zones: Vec<Zone> = (0..w * h)
            .map(|i| {
                let r = if i % w >= w / 2 { 0.204 } else { 0.2 };
                (r * 100.0, 0.2 * 100.0, 0.2 * 100.0, 100)
            })
            .collect();
        let n = (w * h) as usize;
        let ones = vec![1.0; n];
        let (mut lr, mut lb) = (vec![1.0; n], vec![1.0; n]);
        let s = Settings {
            sigma_cr: 0.05,
            ..settings()
        };
        run(&zones, (w, h), (&ones, &ones), (&mut lr, &mut lb), &s);
        let ratio = |i: usize| zones[i].0 / zones[i].1 * lr[i];
        let (left, right) = (ratio(2 * 8 + 1), ratio(2 * 8 + 6));
        assert!((right / left - 1.0).abs() < 0.01, "{left} {right}");
        assert!(lb.iter().all(|v| (v - 1.0).abs() < 1e-6));
    }

    /// Against the original's iterations (`computeW`, `constructM`, `gaussSeidel2Sor`,
    /// `runMatrixIterations` from the libcamera tree the device runs, compiled on the host with
    /// stand-ins for its array types): same colour ratios in, same gains out.
    #[test]
    fn matches_the_original_iterations() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/data/alsc_gs_reference.json"))
                .unwrap();
        let nums = |v: &serde_json::Value| -> Vec<f64> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap())
                .collect()
        };
        for case in data["cases"].as_array().unwrap() {
            let c = nums(&case["c"]);
            let want = nums(&case["lambda"]);
            let s = Settings {
                n_iter: case["n_iter"].as_u64().unwrap() as u32,
                ..settings()
            };
            let mut lambda = vec![1.0; c.len()];
            iterate(&c, &mut lambda, 32, case["sigma"].as_f64().unwrap(), &s);
            let worst = lambda
                .iter()
                .zip(&want)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1e-12, "largest difference {worst}");
        }
    }

    #[test]
    fn leaves_distinct_colours_and_empty_zones_alone() {
        let (w, h) = (4u32, 4u32);
        // Two very different colours: no weight between them, nothing to even out.
        let zones: Vec<Zone> = (0..w * h)
            .map(|i| {
                if i == 5 {
                    (0.0, 0.0, 0.0, 0)
                } else if i % w >= 2 {
                    (40.0, 20.0, 10.0, 100)
                } else {
                    (10.0, 20.0, 40.0, 100)
                }
            })
            .collect();
        let n = (w * h) as usize;
        let ones = vec![1.0; n];
        let (mut lr, mut lb) = (vec![1.0; n], vec![1.0; n]);
        run(
            &zones,
            (w, h),
            (&ones, &ones),
            (&mut lr, &mut lb),
            &settings(),
        );
        assert!(lr.iter().chain(&lb).all(|v| (0.95..=1.05).contains(v)));
        let (cr, _) = cr_cb(&zones, &settings());
        assert_eq!(cr[5], INSUFFICIENT);
    }
}
