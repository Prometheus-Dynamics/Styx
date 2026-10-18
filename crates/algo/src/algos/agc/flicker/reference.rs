//! Test-only reference: the flicker fit as it was before the per-sample trigonometry cache
//! (Styx commit 5d0ffb3, copied verbatim apart from this header and the inline `fit` module).
//! The randomized comparison in [`super::compare`] runs it next to the incremental fit.

#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::f64::consts::PI;

use fit::{Fit, Scratch, depth, fit_samples};

/// Mains frequencies.
pub const MAINS_HZ: [f64; 2] = [50.0, 60.0];
/// Harmonics of the mains frequency fitted (50, 100 and 150 Hz on 50 Hz mains).
pub const HARMONICS: usize = 3;
/// Seconds of frames a fit uses.
pub const WINDOW: f64 = 1.0;
/// Most frames a fit uses.
pub const MAX_SAMPLES: usize = 64;
/// Fewest frames a fit needs.
pub const MIN_SAMPLES: usize = 8;
/// Degrees of freedom left for the residual at least.
const MIN_DOF: usize = 4;
/// A column is used when at least this part of it is not explained by the columns before.
const MIN_INDEPENDENT: f64 = 0.1;
/// A column is used when its mean square is at least this (the exposures see the harmonic).
const MIN_COLUMN: f64 = 1e-4;
/// A harmonic is used when the frames sample its phase: the mean of its unit phasors is
/// shorter than this (1: every frame on the same phase, a harmonic aliased to zero; its
/// columns would only follow the exposure, which any non-linearity also does).
const MAX_PHASE_RESULTANT: f64 = 0.8;
/// Significance: the F statistic of the flicker terms.
const MIN_F: f64 = 12.0;
/// Significance: the modulation the frames show, at least this (relative amplitude).
const MIN_VISIBLE: f64 = 0.004;
/// A harmonic with at least this relative amplitude in the light counts for the quantisation
/// period ([`FlickerFit::avoid_period`]).
const MIN_DEPTH: f64 = 0.02;
/// A frame this far (relative) from the model is a change of the scene.
const SCENE_CHANGE: f64 = 0.25;
/// Before there is a model, a frame this far from the mean of the frames so far (which deep
/// flicker on short exposures can move by more than [`SCENE_CHANGE`]).
const SCENE_CHANGE_UNFITTED: f64 = 0.6;
/// Frequency tracking: the fit is tried this far (Hz) either side of the current frequency.
const TRACK_STEP: f64 = 0.05;
/// Frequency tracking: the part of the step to the residual's minimum taken per frame.
const TRACK_GAIN: f64 = 0.3;
/// Frequency tracking: the most the fundamental may be off its nominal value (Hz).
const MAX_OFFSET: f64 = 0.4;
/// Frequency tracking: fits at least this strong (F statistic, modulation, frames).
const TRACK_F: f64 = 30.0;
const TRACK_VISIBLE: f64 = 0.008;
const TRACK_SAMPLES: usize = 16;

#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Time of the exposure's centre, seconds.
    centre: f64,
    /// Exposure, seconds.
    exposure: f64,
    /// Brightness per unit exposure.
    level: f64,
    /// Time of the frame start, seconds.
    time: f64,
}

/// A fitted flicker model.
#[derive(Debug, Clone, PartialEq)]
pub struct FlickerModel {
    /// Fundamental frequency fitted (the mains frequency as tracked), Hz.
    pub hz: f64,
    /// Time (seconds, the fit's clock) the phases are relative to: the newest frame's exposure
    /// centre.
    pub reference: f64,
    /// Mean brightness per unit exposure.
    pub level: f64,
    /// The harmonics in the fit: `(k, a_k, b_k)`, the light's modulation relative to its mean
    /// (`a_k cos kω(t − reference) + b_k sin kω(t − reference)`).
    pub terms: Vec<(usize, f64, f64)>,
    /// F statistic of the flicker terms against a constant.
    pub f_stat: f64,
    /// Modulation the fitted frames show (√2 × the RMS of the fitted flicker).
    pub visible: f64,
    /// Frames fitted.
    pub samples: usize,
    /// Seconds from the first frame fitted to the last.
    pub span: f64,
    /// Residual sum of squares of the fit (brightness relative to the mean).
    pub rss: f64,
    /// The modulation is significant (see the [module documentation](self)).
    pub significant: bool,
}

impl FlickerModel {
    /// A model with no fit yet, its terms' buffer sized for `harmonics` harmonics.
    fn with_harmonics(harmonics: usize) -> Self {
        Self {
            hz: 0.0,
            reference: 0.0,
            level: 0.0,
            terms: Vec::with_capacity(harmonics),
            f_stat: 0.0,
            visible: 0.0,
            samples: 0,
            span: 0.0,
            rss: 0.0,
            significant: false,
        }
    }

    /// The light's modulation depth at harmonic `k` (0 when not in the fit).
    pub fn depth(&self, k: usize) -> f64 {
        depth(&self.terms, k)
    }

    /// How much brighter (relative) than the mean light a frame whose exposure `exposure` is
    /// centred on `centre` (the fit's clock) is.
    pub fn modulation(&self, centre: f64, exposure: f64) -> f64 {
        modulation(self.hz, &self.terms, centre - self.reference, exposure)
    }
}

/// `Σ_k sinc(kωT/2) (a_k cos kωt + b_k sin kωt)` for an exposure `exposure` centred on `t`.
pub(super) fn modulation(hz: f64, terms: &[(usize, f64, f64)], t: f64, exposure: f64) -> f64 {
    let w = 2.0 * PI * hz;
    terms
        .iter()
        .map(|&(k, a, b)| {
            let kw = k as f64 * w;
            let (sn, cn) = (kw * t).rem_euclid(2.0 * PI).sin_cos();
            sinc(kw * exposure / 2.0) * (a * cn + b * sn)
        })
        .sum()
}

/// Least-squares fit of mains flicker at one mains frequency. See the
/// [module documentation](self).
///
/// A fit allocates nothing once the frames fill the window: its buffers are sized from the
/// configuration when the fit is made (see [`Scratch`]), and the model is updated in place.
#[derive(Debug, Clone)]
pub struct FlickerFit {
    mains: f64,
    /// Tracked offset of the fundamental from `mains`, Hz.
    offset: f64,
    harmonics: usize,
    samples: VecDeque<Sample>,
    model: Option<FlickerModel>,
    /// The model last dropped, kept for its buffers (a fit that fails and then succeeds again).
    spare: Option<FlickerModel>,
    scratch: Scratch,
}

/// `sin(x) / x`.
pub(super) fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 { 1.0 } else { x.sin() / x }
}

impl FlickerFit {
    /// A fit for light flickering at harmonics `1..=harmonics` of `mains` hertz.
    pub fn new(mains: f64, harmonics: usize) -> Self {
        let harmonics = harmonics.max(1);
        Self {
            mains,
            offset: 0.0,
            harmonics,
            samples: VecDeque::with_capacity(MAX_SAMPLES + 1),
            model: None,
            spare: None,
            scratch: Scratch::new(harmonics),
        }
    }

    /// The fundamental frequency (nominal).
    pub fn mains(&self) -> f64 {
        self.mains
    }

    /// The fundamental frequency as tracked (see the [module documentation](self)).
    pub fn hz(&self) -> f64 {
        self.mains + self.offset
    }

    /// Forget all frames (the tracked frequency is kept: the mains did not change).
    pub fn reset(&mut self) {
        self.samples.clear();
        self.drop_model();
    }

    /// Drops the model, keeping its buffers for the next one.
    fn drop_model(&mut self) {
        if let Some(m) = self.model.take() {
            self.spare = Some(m);
        }
    }

    /// The latest model.
    pub fn model(&self) -> Option<&FlickerModel> {
        self.model.as_ref()
    }

    /// The latest model, if significant.
    pub fn significant(&self) -> Option<&FlickerModel> {
        self.model.as_ref().filter(|m| m.significant)
    }

    /// The flicker period exposures should be whole multiples of: the mains period when the
    /// light has a significant component at an odd harmonic (half-wave lamps), else half of it.
    pub fn avoid_period(&self) -> f64 {
        let odd = self.significant().is_some_and(|m| {
            m.terms
                .iter()
                .any(|t| t.0 % 2 == 1 && t.1.hypot(t.2) >= MIN_DEPTH)
        });
        if odd {
            1.0 / self.mains
        } else {
            0.5 / self.mains
        }
    }

    /// Exposures end at a fixed point of the frame (its end here; any fixed offset goes into
    /// the fitted phases).
    pub(super) fn centre(time: f64, frame_duration: f64, exposure: f64) -> f64 {
        time + frame_duration - exposure / 2.0
    }

    /// How much brighter than the mean light a frame starting at `time` with this exposure is
    /// (1 without a significant model).
    pub fn correction(&self, time: f64, frame_duration: f64, exposure: f64) -> f64 {
        let Some(m) = self.significant() else {
            return 1.0;
        };
        (1.0 + m.modulation(Self::centre(time, frame_duration, exposure), exposure)).max(0.05)
    }

    /// Adds a frame (`level`: its brightness per unit exposure) and refits.
    pub fn add(&mut self, time: f64, frame_duration: f64, exposure: f64, level: f64) {
        if !(level.is_finite() && level > 0.0) {
            return;
        }
        let centre = Self::centre(time, frame_duration, exposure);
        // A frame far from what the model (or the frames so far) expects: the scene changed.
        let expected = match &self.model {
            Some(m) => Some((
                m.level * (1.0 + m.modulation(centre, exposure)),
                SCENE_CHANGE,
            )),
            None if !self.samples.is_empty() => Some((
                self.samples.iter().map(|x| x.level).sum::<f64>() / self.samples.len() as f64,
                SCENE_CHANGE_UNFITTED,
            )),
            None => None,
        };
        if expected.is_some_and(|(e, limit)| (level / e - 1.0).abs() > limit) {
            self.reset();
        }
        self.samples.push_back(Sample {
            centre,
            exposure,
            level,
            time,
        });
        while self.samples.len() > MAX_SAMPLES
            || self.samples.front().is_some_and(|f| time - f.time > WINDOW)
        {
            self.samples.pop_front();
        }
        let fit = self.fit_now(self.hz(), None);
        self.publish(fit);
        // Frequency tracking from a strong fit: the harmonics it has.
        let track = match &self.model {
            Some(m)
                if m.significant
                    && m.f_stat >= TRACK_F
                    && m.visible >= TRACK_VISIBLE
                    && m.samples >= TRACK_SAMPLES =>
            {
                let mut ks = core::mem::take(&mut self.scratch.only);
                ks.clear();
                ks.extend(m.terms.iter().map(|t| t.0));
                Some((m.rss, ks))
            }
            _ => None,
        };
        if let Some((rss, ks)) = track {
            self.track(rss, &ks);
            self.scratch.only = ks;
        }
    }

    /// The fit of the frames at fundamental `hz` (see [`fit_samples`]).
    fn fit_now(&mut self, hz: f64, only: Option<&[usize]>) -> Option<Fit> {
        let FlickerFit {
            samples,
            harmonics,
            scratch,
            ..
        } = self;
        fit_samples(samples, *harmonics, scratch, hz, only)
    }

    /// Makes `fit` the model, reusing the model's buffers.
    fn publish(&mut self, fit: Option<Fit>) {
        let Some(f) = fit else {
            self.drop_model();
            return;
        };
        let mut m = self
            .model
            .take()
            .or_else(|| self.spare.take())
            .unwrap_or_else(|| FlickerModel::with_harmonics(self.harmonics));
        m.hz = f.hz;
        m.reference = f.reference;
        m.level = f.level;
        m.f_stat = f.f_stat;
        m.visible = f.visible;
        m.samples = f.samples;
        m.span = f.span;
        m.rss = f.rss;
        m.significant = f.significant;
        m.terms.clear();
        m.terms.extend_from_slice(&self.scratch.terms);
        self.model = Some(m);
    }

    /// One step of frequency tracking from the fit at the current frequency (residual `rss`,
    /// harmonics `ks`). The trials fit the same harmonics: where two alias onto each other
    /// (50 and 100 Hz at 30 fps) a trial frequency would otherwise let the frames tell them
    /// apart by their drift alone and fit noise.
    fn track(&mut self, rss: f64, ks: &[usize]) {
        let hz = self.hz();
        let (Some(lo), Some(hi)) = (
            self.fit_now(hz - TRACK_STEP, Some(ks)),
            self.fit_now(hz + TRACK_STEP, Some(ks)),
        ) else {
            return;
        };
        let (rl, rh) = (lo.rss, hi.rss);
        let curvature = rl - 2.0 * rss + rh;
        let step = if curvature > 0.0 {
            TRACK_STEP * (rl - rh) / (2.0 * curvature)
        } else if rl < rh {
            -TRACK_STEP
        } else {
            TRACK_STEP
        };
        let step = step.clamp(-TRACK_STEP, TRACK_STEP);
        self.offset = (self.offset + TRACK_GAIN * step).clamp(-MAX_OFFSET, MAX_OFFSET);
    }
}

mod fit {
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
}
