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
//! (the PiSP path's settled rate) are fine; the mains frequency may drift by a few tenths of a
//! hertz within a window.

use std::collections::VecDeque;
use std::f64::consts::PI;

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
    /// Mean brightness per unit exposure.
    pub level: f64,
    /// The harmonics in the fit: `(k, a_k, b_k)`, the light's modulation relative to its mean.
    pub terms: Vec<(usize, f64, f64)>,
    /// F statistic of the flicker terms against a constant.
    pub f_stat: f64,
    /// Modulation the fitted frames show (√2 × the RMS of the fitted flicker).
    pub visible: f64,
    /// Frames fitted.
    pub samples: usize,
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
}

/// Least-squares fit of mains flicker at one mains frequency. See the
/// [module documentation](self).
#[derive(Debug, Clone)]
pub struct FlickerFit {
    mains: f64,
    harmonics: usize,
    samples: VecDeque<Sample>,
    model: Option<FlickerModel>,
}

/// `sin(x) / x`.
fn sinc(x: f64) -> f64 {
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
            harmonics: harmonics.max(1),
            samples: VecDeque::new(),
            model: None,
        }
    }

    /// The fundamental frequency.
    pub fn mains(&self) -> f64 {
        self.mains
    }

    /// Forget all frames.
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

    /// The columns of a frame: `sinc · cos` and `sinc · sin` per harmonic.
    fn columns(&self, centre: f64, exposure: f64) -> Vec<f64> {
        let w = 2.0 * PI * self.mains;
        let mut out = Vec::with_capacity(2 * self.harmonics);
        for k in 1..=self.harmonics {
            let kw = k as f64 * w;
            let s = sinc(kw * exposure / 2.0);
            let (sn, cn) = (kw * centre).rem_euclid(2.0 * PI).sin_cos();
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
    fn centre(time: f64, frame_duration: f64, exposure: f64) -> f64 {
        time + frame_duration - exposure / 2.0
    }

    /// How much brighter than the mean light a frame starting at `time` with this exposure is
    /// (1 without a significant model).
    pub fn correction(&self, time: f64, frame_duration: f64, exposure: f64) -> f64 {
        let Some(m) = self.significant() else {
            return 1.0;
        };
        let cols = self.columns(Self::centre(time, frame_duration, exposure), exposure);
        (1.0 + Self::predict(m, &cols)).max(0.05)
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
                m.level * (1.0 + Self::predict(m, &self.columns(centre, exposure))),
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
        self.model = self.fit();
    }

    fn fit(&self) -> Option<FlickerModel> {
        let n = self.samples.len();
        if n < MIN_SAMPLES {
            return None;
        }
        let nf = n as f64;
        let mean = self.samples.iter().map(|s| s.level).sum::<f64>() / nf;
        let y: Vec<f64> = self.samples.iter().map(|s| s.level / mean).collect();
        // Candidate columns (constant first), sample-major.
        let rows: Vec<Vec<f64>> = self
            .samples
            .iter()
            .map(|s| self.columns(s.centre, s.exposure))
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
        let w = 2.0 * PI * self.mains;
        let sampled = |k: usize| {
            let (c, s) = self.samples.iter().fold((0.0, 0.0), |(c, s), x| {
                let (sn, cn) = (k as f64 * w * x.centre).rem_euclid(2.0 * PI).sin_cos();
                (c + cn, s + sn)
            });
            c.hypot(s) / nf <= MAX_PHASE_RESULTANT
        };
        let sampled: Vec<bool> = (0..=self.harmonics).map(|k| k > 0 && sampled(k)).collect();
        for j in std::iter::once(0).chain(order) {
            // Leave room for the residual.
            if j > 0 && n < used.len() + 1 + MIN_DOF {
                break;
            }
            if j > 0 && !sampled[(j - 1) / 2 + 1] {
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
            level: x[0] * mean,
            terms,
            f_stat,
            visible: 0.0,
            samples: n,
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
