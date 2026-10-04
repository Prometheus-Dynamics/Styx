//! Mains flicker in the frame-to-frame brightness: detection, and a model AE meters against.
//!
//! Written for Styx (no libcamera counterpart: Raspberry Pi's AGC only quantises exposures to
//! the flicker period, which does nothing for exposures shorter than one period).
//!
//! Lamps on mains of frequency `f` (50 or 60 Hz) flicker at `2f` (full-wave rectified), many
//! LED lamps at `f` itself as well (half-wave drivers; the OV9782's room: a 50 Hz component
//! of ±23% next to smaller ones at 100 and 150 Hz), so the light is modelled as harmonics `k`
//! of the mains frequency, `L(t) = L · (1 + Σ a_k cos kωt + b_k sin kωt)`. A frame integrates
//! the light over its exposure `T`; one whose exposure is centred on `t` sees
//!
//! ```text
//! b(t, T) = L · (1 + Σ_k sinc(k ω T / 2) · (a_k cos kωt + b_k sin kωt)),   ω = 2π f
//! ```
//!
//! Exposures of whole periods of a harmonic see none of it (`sinc = 0`: what [`super::Agc`]'s
//! period quantisation uses); shorter ones see `|sinc|` of it, sampled at the frame rate: on a
//! global-shutter sensor the whole frame beats at the alias frequencies (100 Hz at 120 fps:
//! 20 Hz; 50 Hz at 120 fps: 50 Hz), on a rolling shutter the frame mean does so too. AE
//! metering such frames chases the beat.
//!
//! [`FlickerFit`] keeps the brightness of recent frames normalised by what exposed them
//! (`mean luma / (exposure × gain)`, so AE's own changes drop out), with each frame's time
//! from the frame numbers and durations, and fits `L, a_k, b_k` by least squares (QR by
//! Gram-Schmidt). A harmonic whose columns the frames cannot tell apart from the ones already
//! in (aliased to zero, e.g. 120 Hz at 120 fps, or onto another harmonic, e.g. 50 and 100 Hz
//! both at ±10 Hz at 30 fps) or that the exposures do not see is left out: its effect, if
//! any, is in the others or nowhere. The fit is *significant* when the flicker terms explain
//! the frames clearly better than a constant (an F test) and the modulation they show is not
//! negligible; [`FlickerFit::correction`] then says how much brighter than the mean light a
//! frame is, which AE divides out. Run for both mains frequencies, the fit is also the
//! automatic detection ([`crate::Flicker::Auto`]).
//!
//! Samples are kept for [`WINDOW`] seconds (at most [`MAX_SAMPLES`]); a frame far from the
//! fitted (or, before a fit, the mean) brightness is a change of the scene and restarts the
//! window. Frame times come from frame numbers and durations, so frames the algorithms skip
//! (the PiSP path's settled rate) are fine.
//!
//! The mains frequency is off its nominal value by up to a few tenths of a hertz (and the
//! sensor's clock by some parts per million): measured in the frames' clock on the CM5, the
//! room's 50 Hz was 50.07 Hz. Over a window that is a phase drift of 0.07 × 0.5 s × 360° =
//! 13° at 50 Hz (38° at 150 Hz), which a fit at the nominal frequency averages away and a
//! prediction a few frames ahead (what [`super::deflicker`] needs) gets wrong by several
//! percent. So a strongly significant fit also tracks the frequency: the residual at ±
//! [`TRACK_STEP`] around it, a parabola through the three, and a damped step towards its
//! minimum ([`FlickerFit::hz`]). Phases are fitted relative to the newest frame
//! ([`FlickerModel::reference`]) so frequency changes do not swing them.

use alloc::collections::VecDeque;
use alloc::{vec, vec::Vec};
use core::f64::consts::PI;

use super::{DETECT_F, DETECT_SAMPLES, DETECT_TIME, DETECT_VISIBLE};
use crate::frame::{Deflicker, Flicker, FrameMetadata};
#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use crate::stats::Statistics;

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
    /// The light's modulation depth at harmonic `k` (0 when not in the fit).
    pub fn depth(&self, k: usize) -> f64 {
        self.terms
            .iter()
            .find(|t| t.0 == k)
            .map_or(0.0, |t| t.1.hypot(t.2))
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
#[derive(Debug, Clone)]
pub struct FlickerFit {
    mains: f64,
    /// Tracked offset of the fundamental from `mains`, Hz.
    offset: f64,
    harmonics: usize,
    samples: VecDeque<Sample>,
    model: Option<FlickerModel>,
}

/// `sin(x) / x`.
pub(super) fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 { 1.0 } else { x.sin() / x }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl FlickerFit {
    /// A fit for light flickering at harmonics `1..=harmonics` of `mains` hertz.
    pub fn new(mains: f64, harmonics: usize) -> Self {
        Self {
            mains,
            offset: 0.0,
            harmonics: harmonics.max(1),
            samples: VecDeque::new(),
            model: None,
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
        self.model = None;
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

    /// The columns of a frame whose exposure is centred `t` seconds after the reference:
    /// `sinc · cos` and `sinc · sin` per harmonic of `hz`.
    fn columns(&self, hz: f64, t: f64, exposure: f64) -> Vec<f64> {
        let w = 2.0 * PI * hz;
        let mut out = Vec::with_capacity(2 * self.harmonics);
        for k in 1..=self.harmonics {
            let kw = k as f64 * w;
            let s = sinc(kw * exposure / 2.0);
            let (sn, cn) = (kw * t).rem_euclid(2.0 * PI).sin_cos();
            out.push(s * cn);
            out.push(s * sn);
        }
        out
    }

    fn predict(m: &FlickerModel, cols: &[f64]) -> f64 {
        m.terms
            .iter()
            .map(|&(k, a, b)| a * cols[2 * (k - 1)] + b * cols[2 * (k - 1) + 1])
            .sum()
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
        self.model = self.fit(self.hz(), None);
        if let Some(m) = &self.model
            && m.significant
            && m.f_stat >= TRACK_F
            && m.visible >= TRACK_VISIBLE
            && m.samples >= TRACK_SAMPLES
        {
            let ks: Vec<usize> = m.terms.iter().map(|t| t.0).collect();
            self.track(m.rss, &ks);
        }
    }

    /// One step of frequency tracking from the fit at the current frequency (residual `rss`,
    /// harmonics `ks`). The trials fit the same harmonics: where two alias onto each other
    /// (50 and 100 Hz at 30 fps) a trial frequency would otherwise let the frames tell them
    /// apart by their drift alone and fit noise.
    fn track(&mut self, rss: f64, ks: &[usize]) {
        let hz = self.hz();
        let (Some(lo), Some(hi)) = (
            self.fit(hz - TRACK_STEP, Some(ks)),
            self.fit(hz + TRACK_STEP, Some(ks)),
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

    /// The fit at fundamental `hz`, of the harmonics `only` (all of them: `None`) that the
    /// frames can tell apart.
    fn fit(&self, hz: f64, only: Option<&[usize]>) -> Option<FlickerModel> {
        let n = self.samples.len();
        if n < MIN_SAMPLES {
            return None;
        }
        let nf = n as f64;
        let reference = self.samples.back().map_or(0.0, |s| s.centre);
        let span = self.samples.back().map_or(0.0, |s| s.time)
            - self.samples.front().map_or(0.0, |s| s.time);
        let mean = self.samples.iter().map(|s| s.level).sum::<f64>() / nf;
        let y: Vec<f64> = self.samples.iter().map(|s| s.level / mean).collect();
        // Candidate columns (constant first), sample-major.
        let rows: Vec<Vec<f64>> = self
            .samples
            .iter()
            .map(|s| self.columns(hz, s.centre - reference, s.exposure))
            .collect();
        let column = |j: usize| -> Vec<f64> {
            if j == 0 {
                vec![1.0; n]
            } else {
                rows.iter().map(|r| r[j - 1]).collect()
            }
        };
        // Gram-Schmidt over the columns that add something: Q orthonormal, R upper triangular
        // (stored by column).
        let mut q: Vec<Vec<f64>> = Vec::new();
        let mut r: Vec<Vec<f64>> = Vec::new();
        let mut used: Vec<usize> = Vec::new();
        // The second harmonic first (full-wave flicker is the common case), then the
        // fundamental and the rest: what aliases onto an earlier one goes to that one.
        let mut ks: Vec<usize> = (1..=self.harmonics).collect();
        if ks.len() > 1 {
            ks.swap(0, 1);
        }
        let order = ks.iter().flat_map(|k| [2 * k - 1, 2 * k]);
        let w = 2.0 * PI * hz;
        let sampled = |k: usize| {
            let (c, s) = self.samples.iter().fold((0.0, 0.0), |(c, s), x| {
                let (sn, cn) = (k as f64 * w * (x.centre - reference))
                    .rem_euclid(2.0 * PI)
                    .sin_cos();
                (c + cn, s + sn)
            });
            c.hypot(s) / nf <= MAX_PHASE_RESULTANT
        };
        let sampled: Vec<bool> = (0..=self.harmonics).map(|k| k > 0 && sampled(k)).collect();
        for j in core::iter::once(0).chain(order) {
            // Leave room for the residual.
            if j > 0 && n < used.len() + 1 + MIN_DOF {
                break;
            }
            if j > 0
                && (!sampled[(j - 1) / 2 + 1]
                    || only.is_some_and(|ks| !ks.contains(&((j - 1) / 2 + 1))))
            {
                continue;
            }
            let mut v = column(j);
            let norm0 = dot(&v, &v);
            if j > 0 && norm0 / nf < MIN_COLUMN {
                continue;
            }
            let mut rj = Vec::with_capacity(q.len() + 1);
            for qi in &q {
                let c = dot(qi, &v);
                for (x, qx) in v.iter_mut().zip(qi) {
                    *x -= c * qx;
                }
                rj.push(c);
            }
            let norm = dot(&v, &v);
            if j > 0 && norm < MIN_INDEPENDENT * norm0 {
                continue;
            }
            let len = norm.sqrt();
            v.iter_mut().for_each(|x| *x /= len);
            rj.push(len);
            q.push(v);
            r.push(rj);
            used.push(j);
        }
        let p = used.len() - 1;
        if p == 0 {
            return None;
        }
        // Coefficients: R x = Qᵀ y.
        let qty: Vec<f64> = q.iter().map(|qi| dot(qi, &y)).collect();
        let mut x = vec![0.0; used.len()];
        for i in (0..used.len()).rev() {
            let s: f64 = (i + 1..used.len()).map(|k| r[k][i] * x[k]).sum();
            x[i] = (qty[i] - s) / r[i][i];
        }
        if x[0] <= 0.0 {
            return None;
        }
        let yy = dot(&y, &y);
        let rss1 = (yy - qty.iter().map(|v| v * v).sum::<f64>()).max(1e-30);
        let rss0 = (yy - qty[0] * qty[0]).max(rss1);
        let dof = (n - 1 - p) as f64;
        let f_stat = ((rss0 - rss1) / p as f64) / (rss1 / dof);
        let mut terms: Vec<(usize, f64, f64)> = Vec::new();
        for (i, &j) in used.iter().enumerate().skip(1) {
            let k = (j - 1) / 2 + 1;
            let cos = (j - 1) % 2 == 0;
            let v = x[i] / x[0];
            match terms.iter_mut().find(|t| t.0 == k) {
                Some(t) if cos => t.1 = v,
                Some(t) => t.2 = v,
                None if cos => terms.push((k, v, 0.0)),
                None => terms.push((k, 0.0, v)),
            }
        }
        let mut model = FlickerModel {
            hz,
            reference,
            level: x[0] * mean,
            terms,
            f_stat,
            visible: 0.0,
            samples: n,
            span,
            rss: rss1,
            significant: false,
        };
        let ms = rows
            .iter()
            .map(|c| Self::predict(&model, c).powi(2))
            .sum::<f64>()
            / nf;
        model.visible = (2.0 * ms).sqrt();
        model.significant = f_stat >= MIN_F
            && model.visible >= MIN_VISIBLE
            && (1..=self.harmonics).all(|k| model.depth(k) <= 1.5);
        Some(model)
    }
}

/// AGC's flicker state: the fits it feeds, detection and the periods it quantises to.
impl super::Agc {
    /// The flicker avoidance the fits and the detection work as: [`Flicker::Auto`] when
    /// avoidance is off but deflicker is on (exposures are then not quantised: see
    /// [`super::Agc::divide`]).
    pub(super) fn fitting(flicker: Flicker, deflicker: Deflicker) -> Flicker {
        if flicker == Flicker::Off && deflicker == Deflicker::On {
            Flicker::Auto
        } else {
            flicker
        }
    }

    /// The mains frequencies to fit for these controls.
    pub(super) fn flicker_mains(flicker: Flicker) -> Vec<f64> {
        match flicker {
            Flicker::Off => Vec::new(),
            Flicker::Auto => MAINS_HZ.to_vec(),
            // A light flicker period: the mains it comes from (full-wave) is twice as long.
            f => f
                .period()
                .map(|p| vec![0.5 / p.as_secs_f64()])
                .unwrap_or_default(),
        }
    }

    /// The start time of `meta`'s frame (seconds), from the frame numbers and durations.
    pub(super) fn frame_time(&mut self, meta: &FrameMetadata) -> f64 {
        let fd = meta.frame_duration.as_secs_f64();
        let t = match self.clock {
            Some((f, t)) if meta.frame > f => t + (meta.frame - f) as f64 * fd,
            Some((f, t)) if meta.frame == f => t,
            _ => 0.0,
        };
        self.clock = Some((meta.frame, t));
        t
    }

    /// Feeds the flicker fits with this frame, works out the flicker periods to avoid and
    /// returns the mains frequency in use and how much brighter than the mean light the frame
    /// is. The fit of the mains in use is `self.in_use` afterwards; `self.clock` has the
    /// frame's time.
    pub(super) fn flicker(
        &mut self,
        stats: &Statistics,
        meta: &FrameMetadata,
        usable: bool,
    ) -> (Option<f64>, f64) {
        let flicker = Self::fitting(meta.controls.flicker, meta.controls.deflicker);
        let wanted = Self::flicker_mains(flicker);
        if self.fits.len() != wanted.len()
            || self.fits.iter().zip(&wanted).any(|(f, w)| f.mains() != *w)
        {
            self.fits = wanted
                .iter()
                .map(|&hz| FlickerFit::new(hz, HARMONICS))
                .collect();
            self.detect_since = vec![None; wanted.len()];
        }
        if flicker != Flicker::Auto {
            self.detected = None;
        }
        let time = self.frame_time(meta);
        let (fd, t) = (
            meta.frame_duration.as_secs_f64(),
            meta.exposure.as_secs_f64(),
        );
        let total = t * meta.analogue_gain * meta.digital_gain.max(1e-9);
        let y = stats.mean_luma();
        if usable && total > 0.0 && (0.005..0.85).contains(&y) {
            for f in &mut self.fits {
                f.add(time, fd, t, y / total);
            }
        }
        if flicker == Flicker::Auto {
            for (i, f) in self.fits.iter().enumerate() {
                let strong = f.significant().is_some_and(|m| {
                    m.f_stat >= DETECT_F
                        && m.visible >= DETECT_VISIBLE
                        && m.samples >= DETECT_SAMPLES
                });
                self.detect_since[i] = strong.then(|| self.detect_since[i].unwrap_or(time));
            }
            // The strongest of those seen long enough; kept until another one is.
            let best = (0..self.fits.len())
                .filter(|&i| self.detect_since[i].is_some_and(|t0| time - t0 >= DETECT_TIME))
                .max_by(|&a, &b| {
                    let f = |i: usize| self.fits[i].model().map_or(0.0, |m| m.f_stat);
                    f(a).total_cmp(&f(b))
                });
            if let Some(i) = best {
                self.detected = Some(self.fits[i].mains());
            }
        }
        let mains = match flicker {
            Flicker::Auto => self.detected,
            _ => self.fits.first().map(FlickerFit::mains),
        };
        self.in_use = mains.and_then(|hz| self.fits.iter().position(|f| f.mains() == hz));
        let fit = self.in_use.map(|i| &self.fits[i]);
        // Whole mains periods when the lamp flickers at the mains frequency too (shorter
        // exposures are left alone: whole half periods would not cancel that flicker and
        // longer exposures see less of it; measured at 60 fps under such a lamp, 10 ms
        // exposures flickered 9.9% where 16.5 ms ones did 3.7%), else whole half periods.
        // Kept once seen: whole mains periods hide that flicker from the fit.
        let period = fit.map(FlickerFit::avoid_period);
        let full = mains.map(|hz| 1.0 / hz);
        if period.is_some() && period == full {
            self.mains_period_seen = full;
        } else if self.mains_period_seen != full {
            self.mains_period_seen = None;
        }
        self.periods = if meta.controls.flicker == Flicker::Off {
            Vec::new()
        } else {
            self.mains_period_seen.or(period).into_iter().collect()
        };
        // Until a mains frequency is detected, AE meters against the most significant fit
        // (its own prediction of these frames); exposures are quantised only once detected.
        let provisional = || {
            self.fits
                .iter()
                .filter(|f| f.significant().is_some())
                .max_by(|a, b| {
                    let f = |x: &FlickerFit| x.model().map_or(0.0, |m| m.f_stat);
                    f(a).total_cmp(&f(b))
                })
        };
        let correction = fit
            .or_else(|| (flicker == Flicker::Auto).then(provisional).flatten())
            .map_or(1.0, |f| f.correction(time, fd, t));
        (mains, correction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Light flickering at `(hz, depth)` components, a frame starting at `t0` exposed for `t`.
    fn light(parts: &[(f64, f64)], t0: f64, t: f64) -> f64 {
        1.0 + parts
            .iter()
            .map(|&(hz, depth)| {
                let w = 2.0 * PI * hz;
                depth * ((w * (t0 + t)).sin() - (w * t0).sin()) / (w * t)
            })
            .sum::<f64>()
    }

    /// Frames at `fps` with exposure `t` under the light.
    fn feed(fit: &mut FlickerFit, fps: f64, t: f64, parts: &[(f64, f64)], frames: usize) {
        let fd = 1.0 / fps;
        for i in 0..frames {
            let t0 = i as f64 * fd + fd - t;
            fit.add(i as f64 * fd, fd, t, 0.3 * light(parts, t0, t));
        }
    }

    #[test]
    fn finds_the_modulation_and_predicts_frames() {
        let half_wave = [(50.0, 0.25), (100.0, 0.1), (150.0, 0.03)];
        let full_wave = [(100.0, 0.3)];
        for (fps, t, parts) in [
            (120.0, 0.008, &full_wave[..]),
            (60.0, 0.004, &full_wave[..]),
            (30.0, 0.015, &full_wave[..]),
            (120.0, 0.008, &half_wave[..]),
            (60.0, 0.0165, &half_wave[..]),
            (30.0, 0.025, &half_wave[..]),
        ] {
            let mut f = FlickerFit::new(50.0, HARMONICS);
            feed(&mut f, fps, t, parts, 60);
            let m = f
                .significant()
                .cloned()
                .unwrap_or_else(|| panic!("{fps}: {:?}", f.model()));
            // The next frame's brightness, predicted.
            let fd = 1.0 / fps;
            let truth = light(parts, 60.0 * fd + fd - t, t);
            let pred = f.correction(60.0 * fd, fd, t);
            assert!(
                (pred - truth).abs() < 0.003,
                "{fps}: {pred} vs {truth} ({m:?})"
            );
            // At 30 fps 50 and 100 Hz both alias to ±10 Hz and cannot be told apart (the
            // fit gives both to 100 Hz); elsewhere a lamp flickering at 50 Hz is seen as such.
            let want = if parts.len() > 1 && fps > 30.0 {
                0.02
            } else {
                0.01
            };
            assert!((f.avoid_period() - want).abs() < 1e-12, "{fps}: {m:?}");
        }
    }

    #[test]
    fn nothing_without_flicker_or_without_phase_spread() {
        let mut f = FlickerFit::new(50.0, HARMONICS);
        feed(&mut f, 120.0, 0.008, &[], 60);
        assert!(f.significant().is_none(), "{:?}", f.model());
        // 120 Hz light at 120 fps: every frame on the same phase.
        let mut f = FlickerFit::new(60.0, 2);
        feed(&mut f, 120.0, 0.004, &[(120.0, 0.5)], 60);
        assert!(f.significant().is_none(), "{:?}", f.model());
        // Exposures of whole periods: nothing to see.
        let mut f = FlickerFit::new(50.0, 2);
        feed(&mut f, 30.0, 0.02, &[(100.0, 0.5)], 40);
        assert!(f.model().is_none(), "{:?}", f.model());
        // The other mains frequency is not mistaken for this one.
        let mut f = FlickerFit::new(60.0, HARMONICS);
        feed(&mut f, 90.0, 0.006, &[(100.0, 0.5)], 60);
        assert!(f.significant().is_none(), "{:?}", f.model());
    }

    #[test]
    fn a_scene_change_restarts_the_window() {
        let mut f = FlickerFit::new(50.0, HARMONICS);
        feed(&mut f, 120.0, 0.008, &[(100.0, 0.3)], 40);
        assert!(f.significant().is_some());
        f.add(41.0 / 120.0, 1.0 / 120.0, 0.008, 0.9);
        assert!(f.model().is_none());
    }
}
