//! The least-squares fit of one mains frequency's flicker: the columns of the frames, their
//! orthogonalisation (Gram-Schmidt, into buffers sized once), the coefficients and the
//! significance. See the [module documentation](super).

use alloc::collections::VecDeque;
use alloc::{vec, vec::Vec};
use core::f64::consts::PI;

use super::{
    MAX_PHASE_RESULTANT, MAX_SAMPLES, MIN_COLUMN, MIN_DOF, MIN_F, MIN_INDEPENDENT, MIN_SAMPLES,
    MIN_VISIBLE, Sample, sinc,
};
#[cfg(not(feature = "std"))]
use crate::math::Float as _;

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The scalars of a fit (its terms are in the scratch until the model takes them).
#[derive(Debug, Clone, Copy)]
pub(super) struct Fit {
    pub(super) hz: f64,
    pub(super) reference: f64,
    pub(super) level: f64,
    pub(super) f_stat: f64,
    pub(super) visible: f64,
    pub(super) samples: usize,
    pub(super) span: f64,
    pub(super) rss: f64,
    pub(super) significant: bool,
}

/// The buffers of a fit, sized once for [`MAX_SAMPLES`] frames and `harmonics` harmonics.
#[derive(Debug, Clone)]
pub(super) struct Scratch {
    /// Brightness of each frame relative to the mean.
    pub(super) y: Vec<f64>,
    /// Candidate columns of each frame, frame-major (`2 × harmonics` each).
    pub(super) rows: Vec<f64>,
    /// Orthonormal columns Q, column-major (`n` each).
    pub(super) q: Vec<f64>,
    /// R, column-major with columns `2 × harmonics + 1` long: column `j` holds the coefficients
    /// of Q's columns `i < j` in entries `i`, and its own norm in entry `j`.
    pub(super) r: Vec<f64>,
    /// The column being orthogonalised.
    pub(super) v: Vec<f64>,
    /// The candidate column indices to try, in order (0: the constant).
    pub(super) order: Vec<usize>,
    /// The columns used, in order (0: the constant).
    pub(super) used: Vec<usize>,
    /// Whether the frames sample each harmonic's phase (index `k`).
    pub(super) sampled: Vec<bool>,
    /// Qᵀ y.
    pub(super) qty: Vec<f64>,
    /// The coefficients.
    pub(super) x: Vec<f64>,
    /// The fitted flicker terms (the model's `terms` once published).
    pub(super) terms: Vec<(usize, f64, f64)>,
    /// The harmonics a frequency-tracking trial keeps.
    pub(super) only: Vec<usize>,
}

impl Scratch {
    pub(super) fn new(harmonics: usize) -> Self {
        let cols = 2 * harmonics + 1;
        Self {
            y: Vec::with_capacity(MAX_SAMPLES),
            rows: Vec::with_capacity(MAX_SAMPLES * 2 * harmonics),
            q: Vec::with_capacity(MAX_SAMPLES * cols),
            r: vec![0.0; cols * cols],
            v: Vec::with_capacity(MAX_SAMPLES),
            order: Vec::with_capacity(cols),
            used: Vec::with_capacity(cols),
            sampled: Vec::with_capacity(harmonics + 1),
            qty: Vec::with_capacity(cols),
            x: Vec::with_capacity(cols),
            terms: Vec::with_capacity(harmonics),
            only: Vec::with_capacity(harmonics),
        }
    }
}

/// Appends the columns of a frame whose exposure is centred `t` seconds after the reference:
/// `sinc · cos` and `sinc · sin` per harmonic `1..=harmonics` of `hz`.
fn push_columns(hz: f64, t: f64, exposure: f64, harmonics: usize, out: &mut Vec<f64>) {
    let w = 2.0 * PI * hz;
    for k in 1..=harmonics {
        let kw = k as f64 * w;
        let s = sinc(kw * exposure / 2.0);
        let (sn, cn) = (kw * t).rem_euclid(2.0 * PI).sin_cos();
        out.push(s * cn);
        out.push(s * sn);
    }
}

/// The prediction of the terms for a frame's columns.
fn predict(terms: &[(usize, f64, f64)], cols: &[f64]) -> f64 {
    terms
        .iter()
        .map(|&(k, a, b)| a * cols[2 * (k - 1)] + b * cols[2 * (k - 1) + 1])
        .sum()
}

/// The fit of the frames `samples` at fundamental `hz`, of the harmonics `only` (all of them:
/// `None`) that the frames can tell apart. Its terms are left in `s.terms`.
pub(super) fn fit_samples(
    samples: &VecDeque<Sample>,
    harmonics: usize,
    s: &mut Scratch,
    hz: f64,
    only: Option<&[usize]>,
) -> Option<Fit> {
    let n = samples.len();
    if n < MIN_SAMPLES {
        return None;
    }
    let nf = n as f64;
    let nc = 2 * harmonics;
    let cols = nc + 1;
    let reference = samples.back().map_or(0.0, |s| s.centre);
    let span = samples.back().map_or(0.0, |s| s.time) - samples.front().map_or(0.0, |s| s.time);
    let mean = samples.iter().map(|s| s.level).sum::<f64>() / nf;
    s.y.clear();
    s.y.extend(samples.iter().map(|x| x.level / mean));
    // Candidate columns (constant first, frame-major).
    s.rows.clear();
    for x in samples {
        push_columns(hz, x.centre - reference, x.exposure, harmonics, &mut s.rows);
    }
    // The phases the frames sample, per harmonic: a harmonic on one phase is aliased to zero.
    let w = 2.0 * PI * hz;
    s.sampled.clear();
    s.sampled.push(false);
    for k in 1..=harmonics {
        let (c, sn) = samples.iter().fold((0.0, 0.0), |(c, sn), x| {
            let (sin, cos) = (k as f64 * w * (x.centre - reference))
                .rem_euclid(2.0 * PI)
                .sin_cos();
            (c + cos, sn + sin)
        });
        s.sampled.push(c.hypot(sn) / nf <= MAX_PHASE_RESULTANT);
    }
    // The second harmonic first (full-wave flicker is the common case), then the fundamental and
    // the rest: what aliases onto an earlier one goes to that one.
    s.order.clear();
    s.order.push(0);
    for pos in 0..harmonics {
        let k = match pos {
            0 if harmonics > 1 => 2,
            1 if harmonics > 1 => 1,
            _ => pos + 1,
        };
        s.order.push(2 * k - 1);
        s.order.push(2 * k);
    }
    // Gram-Schmidt over the columns that add something: Q orthonormal, R upper triangular.
    s.used.clear();
    s.q.clear();
    for idx in 0..s.order.len() {
        let j = s.order[idx];
        let nq = s.used.len();
        // Leave room for the residual.
        if j > 0 && n < nq + 1 + MIN_DOF {
            break;
        }
        if j > 0 {
            let k = (j - 1) / 2 + 1;
            if !s.sampled[k] || only.is_some_and(|ks| !ks.contains(&k)) {
                continue;
            }
        }
        s.v.clear();
        if j == 0 {
            s.v.resize(n, 1.0);
        } else {
            for i in 0..n {
                s.v.push(s.rows[i * nc + j - 1]);
            }
        }
        let norm0 = dot(&s.v, &s.v);
        if j > 0 && norm0 / nf < MIN_COLUMN {
            continue;
        }
        let base = nq * cols;
        for i in 0..nq {
            let c = dot(&s.q[i * n..(i + 1) * n], &s.v);
            for (x, qx) in s.v.iter_mut().zip(&s.q[i * n..(i + 1) * n]) {
                *x -= c * qx;
            }
            s.r[base + i] = c;
        }
        let norm = dot(&s.v, &s.v);
        if j > 0 && norm < MIN_INDEPENDENT * norm0 {
            continue;
        }
        let len = norm.sqrt();
        s.v.iter_mut().for_each(|x| *x /= len);
        s.r[base + nq] = len;
        s.q.extend_from_slice(&s.v);
        s.used.push(j);
    }
    let nq = s.used.len();
    let p = nq - 1;
    if p == 0 {
        return None;
    }
    // Coefficients: R x = Qᵀ y.
    s.qty.clear();
    for i in 0..nq {
        let qty = dot(&s.q[i * n..(i + 1) * n], &s.y);
        s.qty.push(qty);
    }
    s.x.clear();
    s.x.resize(nq, 0.0);
    for i in (0..nq).rev() {
        let sum: f64 = (i + 1..nq).map(|k| s.r[k * cols + i] * s.x[k]).sum();
        s.x[i] = (s.qty[i] - sum) / s.r[i * cols + i];
    }
    if s.x[0] <= 0.0 {
        return None;
    }
    let yy = dot(&s.y, &s.y);
    let rss1 = (yy - s.qty.iter().map(|v| v * v).sum::<f64>()).max(1e-30);
    let rss0 = (yy - s.qty[0] * s.qty[0]).max(rss1);
    let dof = (n - 1 - p) as f64;
    let f_stat = ((rss0 - rss1) / p as f64) / (rss1 / dof);
    s.terms.clear();
    for i in 1..nq {
        let j = s.used[i];
        let k = (j - 1) / 2 + 1;
        let cos = (j - 1).is_multiple_of(2);
        let v = s.x[i] / s.x[0];
        match s.terms.iter_mut().find(|t| t.0 == k) {
            Some(t) if cos => t.1 = v,
            Some(t) => t.2 = v,
            None if cos => s.terms.push((k, v, 0.0)),
            None => s.terms.push((k, 0.0, v)),
        }
    }
    let ms = (0..n)
        .map(|i| predict(&s.terms, &s.rows[i * nc..(i + 1) * nc]).powi(2))
        .sum::<f64>()
        / nf;
    let visible = (2.0 * ms).sqrt();
    let significant = f_stat >= MIN_F
        && visible >= MIN_VISIBLE
        && (1..=harmonics).all(|k| depth(&s.terms, k) <= 1.5);
    Some(Fit {
        hz,
        reference,
        level: s.x[0] * mean,
        f_stat,
        visible,
        samples: n,
        span,
        rss: rss1,
        significant,
    })
}

/// The modulation depth at harmonic `k` of `terms` (0 when not in them).
pub(super) fn depth(terms: &[(usize, f64, f64)], k: usize) -> f64 {
    terms
        .iter()
        .find(|t| t.0 == k)
        .map_or(0.0, |t| t.1.hypot(t.2))
}
