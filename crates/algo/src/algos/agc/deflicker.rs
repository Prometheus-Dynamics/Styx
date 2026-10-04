//! Deflicker: the flicker AE's model predicts for a frame, taken out with that frame's ISP
//! digital gain.
//!
//! Written for Styx. Flicker avoidance (whole flicker periods, [`super::flicker`]) needs
//! exposures of at least a period; shorter ones (all of them at 120 fps, 8 ms) keep the
//! flicker in the frames: under the OV9782's room lamp the output's brightness moved by 8-12%
//! frame to frame although AE no longer chased it. The fitted model says how much brighter
//! than the mean light any exposure window is, so the ISP can divide it out:
//!
//! * **Per frame.** [`FlickerCorrection`] is the model as of the frame the algorithms last ran
//!   on, with that frame's place on the fit's clock. The ISP settings for a frame (which on
//!   the PiSP path come from the frame before, and at the settled rate from further back) ask
//!   it for *that* frame's gain ([`FlickerCorrection::gain`]) with the frame's own number,
//!   duration and exposure: the prediction runs a frame or a few ahead of the fit, which the
//!   frequency tracking of the fit keeps in phase.
//! * **Global and rolling shutters.** A global shutter (the OV9782) exposes every row over the
//!   same window: one factor per frame. On a rolling shutter each row's window starts a line
//!   later, so [`FlickerCorrection::band_gains`] gives a gain per band of rows on top of the
//!   frame's (`CameraConfig::readout`; the pipeline folds them into the lens shading grid).
//! * **Confidence.** Only a fit that is clearly significant (F ≥ 30, at least 16 frames,
//!   sampled on most frames, modulation ≥ 1%) turns the correction on; it fades in and out
//!   over [`FADE`] seconds, so a scene change (which restarts the fit) or the flicker going
//!   away never switches it abruptly. The coefficients are low-pass filtered after rotating
//!   the old ones to the new fit's reference time, so the phase moves smoothly.
//! * **Headroom.** A brighter-than-average frame needs a gain below 1, which would turn
//!   highlights that clipped in the raw frame grey (the ISP keeps channel gains at 1 or more
//!   for that reason). Where the frames' highlights (the luma histogram's top 0.1%, with a
//!   margin for colour channels) stay clear of clipping even in the brightest frame, gains
//!   below 1 are harmless and the sensor keeps its exposure. Where they would not, AE asks
//!   the sensor for its total exposure divided by as much as the highlights need, up to the
//!   brightest frame the model predicts ([`FlickerCorrection::headroom`]), and the ISP's
//!   digital gain makes up the rest; frames then never get less than `headroom / peak`, and
//!   what clips in a raw frame is white in the output anyway. The sensor's exposure is cut
//!   only as far as highlights need it (in a dim scene the analogue gain takes the cut, which
//!   costs next to no noise). The highlights move with the flicker and with noise from frame
//!   to frame, so the headroom follows the largest need of the last second or so (a maximum
//!   that decays by [`HEADROOM_DECAY`] per second) and moves only when that is clearly more
//!   or less than it has (hysteresis): the sensor request stays put.
//! * **Hand-off.** An exposure of whole periods sees no flicker (`sinc = 0`): its predicted
//!   brightness is 1 and its gain is AE's alone, so a switch between short and quantised
//!   exposures needs nothing special; the headroom follows within a frame or two.
//!
//! Gains are limited to [`MAX_GAIN`] either way; the correction is off while AE is (manual
//! exposure and gain leave no headroom).

use alloc::vec::Vec;
use core::f64::consts::PI;

use serde::{Deserialize, Serialize};

use super::flicker::{FlickerFit, FlickerModel, modulation, sinc};

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

/// Seconds the correction takes to fade in or out.
pub const FADE: f64 = 0.25;
/// Gains (and predicted brightness factors) are kept within `1 / MAX_GAIN ..= MAX_GAIN`.
pub const MAX_GAIN: f64 = 1.6;
/// Confidence: the fit's F statistic, frames, modulation the frames show.
const MIN_F: f64 = 30.0;
const MIN_SAMPLES: usize = 16;
const MIN_VISIBLE: f64 = 0.01;
/// Confidence: frames fitted at most this many frame durations apart on average (a fit from
/// the settled rate's every eighth frame can alias the harmonics onto each other).
const MAX_SPACING: f64 = 1.5;
/// Part of a new fit's coefficients taken per frame.
const SMOOTHING: f64 = 0.3;
/// Headroom: what the highlights need times this.
const HEADROOM_MARGIN: f64 = 1.02;
/// Headroom: the luma histogram quantile taken as the frame's highlights.
pub(super) const HIGHLIGHT_QUANTILE: f64 = 0.999;
/// Headroom: colour channels may clip before luma does by this much.
const HIGHLIGHT_MARGIN: f64 = 1.25;
/// Headroom: raised to what is needed times this (so small rises need no new request).
const HEADROOM_RAISE: f64 = 1.03;
/// Headroom: lowered only when this much more than needed.
const HEADROOM_DROP: f64 = 1.1;
/// Headroom: the need remembered decays by this much per second.
pub const HEADROOM_DECAY: f64 = 0.1;
/// Rolling shutter: band gains only while the readout keeps at least this much of each
/// harmonic in the frame mean the model was fitted to.
const MIN_READOUT_SINC: f64 = 0.3;

/// The flicker correction for coming frames. See the [module documentation](self).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlickerCorrection {
    /// Fundamental frequency (Hz, as tracked).
    pub hz: f64,
    /// Time (seconds on the fit's clock) the phases are relative to.
    pub reference: f64,
    /// `(k, a_k, b_k)`: the light's modulation at harmonic `k`, as frames' means see it.
    pub terms: Vec<(usize, f64, f64)>,
    /// The frame the algorithms last ran on and its start time on the fit's clock.
    pub clock: (u64, f64),
    /// How much of the correction is applied, 0 to 1 (fading in or out).
    pub strength: f64,
    /// The part of AE's total exposure left to the ISP's digital gain, so no highlight that
    /// clips in the raw frame gets a gain below 1 (1: none).
    pub headroom: f64,
    /// Rolling shutter readout (seconds; 0 for a global shutter).
    pub readout: f64,
}

impl FlickerCorrection {
    /// Whether frames are corrected.
    pub fn active(&self) -> bool {
        self.strength > 0.0 && !self.terms.is_empty()
    }

    /// Start time of frame `frame` on the fit's clock (frames last `frame_duration` seconds
    /// since the clock's frame).
    pub fn frame_time(&self, frame: u64, frame_duration: f64) -> f64 {
        let (f0, t0) = self.clock;
        t0 + (frame as f64 - f0 as f64) * frame_duration
    }

    /// How much brighter than the mean light the model says a frame (or, `row` seconds into
    /// its readout from the middle row, a row of it) is, with the correction's strength.
    fn brightness_at(&self, frame: u64, frame_duration: f64, exposure: f64, row: f64) -> f64 {
        if !self.active() {
            return 1.0;
        }
        let centre = FlickerFit::centre(
            self.frame_time(frame, frame_duration),
            frame_duration,
            exposure,
        );
        let m = modulation(
            self.hz,
            &self.terms,
            centre + row - self.reference,
            exposure,
        );
        (1.0 + self.strength * m).clamp(1.0 / MAX_GAIN, MAX_GAIN)
    }

    /// How much brighter than the mean light frame `frame` (`frame_duration`, `exposure` in
    /// seconds) is, as corrected (1 when inactive).
    pub fn brightness(&self, frame: u64, frame_duration: f64, exposure: f64) -> f64 {
        self.brightness_at(frame, frame_duration, exposure, 0.0)
    }

    /// The gain that takes the flicker out of frame `frame`: 1 / [`Self::brightness`].
    pub fn gain(&self, frame: u64, frame_duration: f64, exposure: f64) -> f64 {
        1.0 / self.brightness(frame, frame_duration, exposure)
    }

    /// Rolling shutter: gains for `bands` bands of rows, top to bottom, on top of
    /// [`Self::gain`] (their effect on the frame's mean is 1, so statistics taken after them
    /// still show the frame's flicker). `None` for a global shutter, while inactive, or when
    /// the readout averages a harmonic away too much to undo.
    pub fn band_gains(
        &self,
        frame: u64,
        frame_duration: f64,
        exposure: f64,
        bands: usize,
    ) -> Option<Vec<f64>> {
        if self.readout <= 0.0 || bands < 2 || !self.active() {
            return None;
        }
        // The fit saw each harmonic through the readout too (the frame mean averages the
        // rows): rows see it undivided.
        let w = 2.0 * PI * self.hz;
        let mut terms = self.terms.clone();
        for t in &mut terms {
            let s = sinc(t.0 as f64 * w * self.readout / 2.0);
            if s.abs() < MIN_READOUT_SINC {
                return None;
            }
            (t.1, t.2) = (t.1 / s, t.2 / s);
        }
        let rows = FlickerCorrection {
            terms,
            ..self.clone()
        };
        let k: Vec<f64> = (0..bands)
            .map(|i| {
                let row = ((i as f64 + 0.5) / bands as f64 - 0.5) * self.readout;
                rows.brightness_at(frame, frame_duration, exposure, row)
            })
            .collect();
        let mean = k.iter().sum::<f64>() / bands as f64;
        Some(k.iter().map(|k| mean / k).collect())
    }

    /// The smallest gain the correction may give a frame exposed for `exposure` seconds:
    /// `headroom / peak`, at most 1.
    pub fn floor(&self, exposure: f64) -> f64 {
        (self.headroom / self.peak(exposure)).min(1.0)
    }

    /// The brightest frame an exposure of `exposure` seconds can be, as corrected.
    pub fn peak(&self, exposure: f64) -> f64 {
        let w = 2.0 * PI * self.hz;
        1.0 + self.strength
            * self
                .terms
                .iter()
                .map(|t| (sinc(t.0 as f64 * w * exposure / 2.0) * t.1.hypot(t.2)).abs())
                .sum::<f64>()
    }
}

/// AGC's deflicker state.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DeflickerState {
    strength: f64,
    headroom: f64,
    /// The largest headroom needed lately (decaying).
    needed: f64,
    /// The smoothed model (its frequency, reference time and terms).
    smoothed: Option<FlickerCorrection>,
    last_time: Option<f64>,
}

impl Default for DeflickerState {
    fn default() -> Self {
        Self {
            strength: 0.0,
            headroom: 1.0,
            needed: 1.0,
            smoothed: None,
            last_time: None,
        }
    }
}

/// Whether a fit is good enough to correct frames with.
fn confident(m: &FlickerModel, frame_duration: f64) -> bool {
    m.significant
        && m.f_stat >= MIN_F
        && m.samples >= MIN_SAMPLES
        && m.visible >= MIN_VISIBLE
        && m.span <= MAX_SPACING * frame_duration * (m.samples - 1) as f64
}

/// `(k, a, b)` relative to `from` re-expressed relative to `to` at `hz`.
fn rotate(terms: &mut [(usize, f64, f64)], hz: f64, from: f64, to: f64) {
    let w = 2.0 * PI * hz;
    for t in terms {
        let (s, c) = (t.0 as f64 * w * (to - from))
            .rem_euclid(2.0 * PI)
            .sin_cos();
        (t.1, t.2) = (t.1 * c + t.2 * s, t.2 * c - t.1 * s);
    }
}

impl DeflickerState {
    /// Updates the state with this frame (number `frame`, starting at `time` on the fit's
    /// clock, `frame_duration` and `exposure` seconds) and the fit of the mains in use;
    /// `enabled`: deflicker on and AE free to leave headroom; `highlight`: the frame's
    /// highlights on the output's scale under the mean light (1: white; infinite when they
    /// clipped). Returns the correction for the coming frames, `None` when there is nothing to
    /// correct (so the algorithms can run at their settled rate).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn update(
        &mut self,
        enabled: bool,
        model: Option<&FlickerModel>,
        (frame, time): (u64, f64),
        frame_duration: f64,
        exposure: f64,
        readout: f64,
        highlight: f64,
    ) -> Option<FlickerCorrection> {
        if !enabled {
            *self = Self::default();
            return None;
        }
        let dt = self.last_time.map_or(0.0, |t| (time - t).clamp(0.0, 0.1));
        self.last_time = Some(time);
        let good = model.filter(|m| confident(m, frame_duration));
        if let Some(m) = good {
            let mut terms = m.terms.clone();
            if let Some(old) = &mut self.smoothed {
                rotate(&mut old.terms, m.hz, old.reference, m.reference);
                for t in &mut terms {
                    let (a, b) = old
                        .terms
                        .iter()
                        .find(|o| o.0 == t.0)
                        .map_or((0.0, 0.0), |o| (o.1, o.2));
                    t.1 = a + SMOOTHING * (t.1 - a);
                    t.2 = b + SMOOTHING * (t.2 - b);
                }
            }
            self.smoothed = Some(FlickerCorrection {
                hz: m.hz,
                reference: m.reference,
                terms,
                clock: (frame, time),
                strength: 1.0,
                headroom: 1.0,
                readout,
            });
        }
        let step = if dt > 0.0 { dt / FADE } else { 1.0 / 16.0 };
        self.strength = if good.is_some() {
            (self.strength + step).min(1.0)
        } else {
            (self.strength - step).max(0.0)
        };
        let Some(smoothed) = &self.smoothed else {
            // Flickering but not (yet) confident: nothing to correct, but the algorithms
            // should see every frame.
            return model.filter(|m| m.significant).map(|m| FlickerCorrection {
                hz: m.hz,
                reference: m.reference,
                terms: Vec::new(),
                clock: (frame, time),
                strength: 0.0,
                headroom: 1.0,
                readout,
            });
        };
        // Headroom for the highlights in the brightest frame at full strength (it does not
        // follow the fade, so fading moves no sensor values).
        let full = FlickerCorrection {
            clock: (frame, time),
            strength: 1.0,
            headroom: 1.0,
            readout,
            ..smoothed.clone()
        };
        if good.is_some() {
            let peak = full.peak(exposure).min(MAX_GAIN);
            let wanted = peak * highlight * HIGHLIGHT_MARGIN;
            let needed = if wanted > 1.0 {
                wanted.min(peak) * HEADROOM_MARGIN
            } else {
                1.0
            };
            self.needed = needed.max(self.needed - HEADROOM_DECAY * dt).max(1.0);
            if self.needed > self.headroom || self.needed * HEADROOM_DROP < self.headroom {
                self.headroom = if self.needed > 1.0 {
                    self.needed * HEADROOM_RAISE
                } else {
                    1.0
                };
            }
        } else if self.strength == 0.0 {
            self.headroom = 1.0;
            self.needed = 1.0;
        }
        if self.strength == 0.0 && !model.is_some_and(|m| m.significant) {
            self.smoothed = None;
            return None;
        }
        Some(FlickerCorrection {
            strength: self.strength,
            headroom: self.headroom,
            ..full
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn correction(readout: f64) -> FlickerCorrection {
        FlickerCorrection {
            hz: 50.0,
            reference: 0.0,
            terms: vec![(1, 0.2, 0.05), (2, 0.05, -0.03)],
            clock: (10, 10.0 / 120.0),
            strength: 1.0,
            headroom: 1.3,
            readout,
        }
    }

    #[test]
    fn gains_undo_the_predicted_brightness() {
        let c = correction(0.0);
        let fd = 1.0 / 120.0;
        for f in 10..30 {
            let b = c.brightness(f, fd, 0.008);
            assert!((b * c.gain(f, fd, 0.008) - 1.0).abs() < 1e-12);
            assert!(b <= c.peak(0.008) + 1e-12, "{b}");
        }
        // Whole periods see nothing.
        assert!((c.brightness(17, fd, 0.02) - 1.0).abs() < 1e-12);
        assert!(c.band_gains(11, fd, 0.008, 8).is_none());
        let off = FlickerCorrection { strength: 0.0, ..c };
        assert_eq!(off.gain(12, fd, 0.008), 1.0);
    }

    #[test]
    fn band_gains_follow_the_rows_and_keep_the_mean() {
        let c = correction(0.006);
        let fd = 1.0 / 30.0;
        let g = c.band_gains(11, fd, 0.004, 16).unwrap();
        // Times the frame's gain, each band's light comes out flat; the bands differ.
        let k: Vec<f64> = (0..16)
            .map(|i| {
                let row = ((i as f64 + 0.5) / 16.0 - 0.5) * 0.006;
                let mut rows = c.clone();
                let w = 2.0 * PI * 50.0;
                for t in &mut rows.terms {
                    let s = sinc(t.0 as f64 * w * 0.003);
                    (t.1, t.2) = (t.1 / s, t.2 / s);
                }
                rows.brightness_at(11, fd, 0.004, row)
            })
            .collect();
        let mean = k.iter().sum::<f64>() / 16.0;
        for (gi, ki) in g.iter().zip(&k) {
            assert!((gi * ki - mean).abs() < 1e-12);
        }
        let spread = g.iter().fold(0.0f64, |a, x| a.max((x - 1.0).abs()));
        assert!(spread > 0.02, "{g:?}");
    }

    #[test]
    fn rotation_keeps_the_waveform() {
        let mut t = vec![(1, 0.2, 0.05), (3, -0.01, 0.02)];
        let before = modulation(50.03, &t, 0.0123, 0.004);
        rotate(&mut t, 50.03, 0.0, 0.5);
        let after = modulation(50.03, &t, 0.0123 - 0.5, 0.004);
        assert!((before - after).abs() < 1e-12);
    }
}
