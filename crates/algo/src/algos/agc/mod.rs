//! AGC / AE: automatic exposure and gain.
//!
//! Ported from Raspberry Pi's `agc_channel.cpp` (BSD-2-Clause, Copyright (C) 2023 Raspberry Pi
//! Ltd), channel 0 only, with these differences:
//!
//! * Output carries the frame it applies from ([`crate::SensorRequest::frame`]), computed from
//!   the sensor's control delays, instead of relying on a separate delayed-controls helper.
//! * The frame duration is chosen here (exposure + margin, within the limits).
//! * EV is in stops; zone weights come from the tuning or built-in shapes (see `metering`).
//! * Model-based steps: the target is a total exposure computed from what actually produced
//!   each frame, and the control delays say when a change lands, so a change larger than
//!   `full_step` goes straight to the target (undamped) at any time, not only at start-up.
//!   Damping (`speed`) is left for small changes, where it keeps noise out of the exposure.
//! * Every frame's statistics are used: a frame produced before a change lands meters the
//!   scene against its own exposure and so asks for the same total again (nothing new is
//!   sent); a different answer (the scene changed) replaces the change in flight.
//! * De-saturation happens only while at least half the image is saturated, without damping,
//!   and continues from the reduced sensor exposure (the original re-derives it from the
//!   digitally compensated total, which overshoots with multi-frame delays).
//! * Locked: no change larger than the lock tolerance (5%) is in flight and the frame meters
//!   within 5% of the target, for [`LOCK_FRAMES`] frames in a row. Once locked, and on the
//!   frames right after unsettled ones ([`CameraConfig::unsettled_frames`], left out
//!   altogether), AE leaves a frame within that tolerance alone (no hunting).
//! * After an undamped change lands, what is left (the model's error: clipped zones, a black
//!   level offset) is corrected at once too.
//! * A warm start ([`crate::WarmStart`], e.g. the last session's values) replaces the tuning's
//!   start-up exposure, re-split for the new mode's limits.
//! * Flicker: besides quantising exposures to whole flicker periods (which needs exposures of
//!   at least one period), AE fits the flicker the frames show ([`flicker`]) and meters each
//!   frame against the mean light, so exposures shorter than a period (8 ms at 120 fps under
//!   100 Hz light) do not chase the beat; [`crate::Flicker::Auto`] detects 50 or 60 Hz mains
//!   from the same fits and then avoids it as if it had been set.
//!
//! Per frame: meter the scene (weighted zone luma, iterated because saturated zones do not
//! scale), apply the histogram constraints, compute the target total exposure relative to what
//! actually produced this frame, damp small changes, split it into exposure time / analogue
//! gain / digital gain along the exposure profile, and snap the exposure time to the flicker
//! period.

pub mod flicker;
mod metering;
pub mod tuning;

use std::time::Duration;

use crate::config::CameraConfig;
use crate::error::Result;
use crate::frame::{Controls, Flicker, FrameMetadata};
use crate::params::{AeStatus, Params, SensorRequest};
use crate::pipeline::Algorithm;
use crate::stats::{Statistics, ZoneGrid};
use crate::warm::WarmStart;

use flicker::FlickerFit;
use tuning::{AgcTuning, Bound, ExposureProfile, MeteringMode};

/// Luma targets are capped here: histograms cannot be read near saturation.
const EV_GAIN_Y_TARGET_LIMIT: f64 = 0.9;
/// Frames in a row that must be settled (nothing in flight, on target) to report a lock.
pub const LOCK_FRAMES: u32 = 2;
/// A frame is on target when the gain it still needs is within this of 1.
const ON_TARGET: f64 = 0.05;
/// Automatic flicker detection: seconds a fit stays this significant (the F statistic of its
/// flicker terms, the modulation the frames show, frames fitted) without a break.
const DETECT_TIME: f64 = 0.5;
const DETECT_F: f64 = 30.0;
const DETECT_SAMPLES: usize = 16;
const DETECT_VISIBLE: f64 = 0.008;

/// The AGC algorithm. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Agc {
    tuning: AgcTuning,
    config: CameraConfig,
    weights: Option<(String, u32, u32, bool, Vec<f64>)>,
    /// The metering mode's weights for the ISP's histogram, by mode name.
    histogram: Option<(String, ZoneGrid<f64>)>,
    frame_count: u32,
    /// Damped total exposure (seconds × gain), with digital gain.
    filtered: f64,
    /// Values AE was frozen at when switched off.
    frozen: Option<(Duration, f64)>,
    /// The last request's exposure and analogue gain.
    last: Option<(Duration, f64)>,
    /// The last request.
    last_request: Option<SensorRequest>,
    /// Frame on which the latest change of the request lands.
    lands_at: u64,
    /// Frame on which the latest change larger than the lock tolerance lands.
    settles_at: u64,
    /// The latest change was undamped; its first frame may correct what is left.
    full_step_pending: bool,
    lock_count: u32,
    /// Start-up exposure and gain from a warm start.
    warm: Option<(f64, f64)>,
    /// The latest frame's number and start time (seconds since the first frame).
    clock: Option<(u64, f64)>,
    /// Flicker fits, one per frequency in use (see [`flicker`]).
    fits: Vec<FlickerFit>,
    /// Automatic detection: since when each fit has been significant.
    detect_since: Vec<Option<f64>>,
    /// The mains frequency detected automatically.
    detected: Option<f64>,
    /// Flicker periods exposures are quantised to (seconds, longest first: the first one the
    /// exposure reaches), for [`Self::divide`].
    periods: Vec<f64>,
    /// The lamp was seen flickering at the mains frequency: its period (seconds).
    mains_period_seen: Option<f64>,
}

/// Exposure split into its parts.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Split {
    exposure: f64,
    analogue_gain: f64,
    digital_gain: f64,
}

impl Agc {
    /// AGC with a tuning.
    pub fn new(tuning: AgcTuning) -> Result<Self> {
        tuning.validate()?;
        Ok(Self {
            tuning,
            config: CameraConfig::default(),
            weights: None,
            histogram: None,
            frame_count: 0,
            filtered: 0.0,
            frozen: None,
            last: None,
            last_request: None,
            lands_at: 0,
            settles_at: 0,
            full_step_pending: false,
            lock_count: 0,
            warm: None,
            clock: None,
            fits: Vec::new(),
            detect_since: Vec::new(),
            detected: None,
            periods: Vec::new(),
            mains_period_seen: None,
        })
    }

    /// The tuning.
    pub fn tuning(&self) -> &AgcTuning {
        &self.tuning
    }

    fn frame_duration_limits(&self, meta: Option<&FrameMetadata>) -> (f64, f64) {
        let (mut lo, mut hi) = self.config.frame_duration_limits;
        if let Some((a, b)) = meta.and_then(|m| m.controls.frame_duration_limits) {
            lo = a.clamp(lo, hi);
            hi = b.clamp(lo, hi);
        }
        (lo.as_secs_f64(), hi.as_secs_f64())
    }

    fn exposure_limits(&self, meta: Option<&FrameMetadata>) -> (f64, f64) {
        let (lo, hi) = self.config.exposure_limits;
        let fd_max = self.frame_duration_limits(meta).1 - self.config.exposure_margin.as_secs_f64();
        let hi = hi.as_secs_f64().min(fd_max).max(lo.as_secs_f64());
        (lo.as_secs_f64(), hi)
    }

    fn limit_exposure(&self, t: f64, meta: Option<&FrameMetadata>) -> f64 {
        let (lo, hi) = self.exposure_limits(meta);
        t.clamp(lo, hi)
    }

    fn limit_gain(&self, g: f64) -> f64 {
        let (lo, hi) = self.config.analogue_gain_limits;
        g.clamp(lo, hi * self.tuning.max_digital_gain)
    }

    fn profile(&self, controls: &Controls) -> &ExposureProfile {
        controls
            .exposure_mode
            .as_ref()
            .and_then(|n| self.tuning.exposure_modes.get(n))
            .unwrap_or(&self.tuning.exposure_modes[&self.tuning.default_exposure_mode])
    }

    /// Fixed exposure time and gain: from the controls, or frozen when AE is off.
    fn fixed(&mut self, meta: &FrameMetadata) -> (Option<f64>, Option<f64>) {
        let c = &meta.controls;
        if c.ae_enable {
            self.frozen = None;
        } else if self.frozen.is_none() {
            self.frozen = self.last.or(Some((meta.exposure, meta.analogue_gain)));
        }
        let frozen = self.frozen;
        let exposure = c
            .exposure
            .or(frozen.map(|f| f.0))
            .map(|e| self.limit_exposure(e.as_secs_f64(), Some(meta)));
        let gain = c
            .analogue_gain
            .or(frozen.map(|f| f.1))
            .map(|g| self.limit_gain(g));
        (exposure, gain)
    }

    /// The metering mode in use (`average` is `matrix` unless tuned) and its tuned weights.
    fn metering_mode(&self, meta: Option<&FrameMetadata>) -> (String, Option<&MeteringMode>) {
        let name = meta
            .and_then(|m| m.controls.metering_mode.clone())
            .unwrap_or_else(|| self.tuning.default_metering_mode.clone());
        let key = if name == "average" && !self.tuning.metering_modes.contains_key("average") {
            "matrix"
        } else {
            name.as_str()
        };
        let tuned = self.tuning.metering_modes.get(key);
        (name, tuned)
    }

    /// The histogram weights for the metering mode in use (see [`Params::histogram_weights`]).
    fn histogram_weights(&mut self, meta: &FrameMetadata) -> ZoneGrid<f64> {
        let (name, tuned) = self.metering_mode(Some(meta));
        if let Some((n, g)) = &self.histogram
            && *n == name
        {
            return g.clone();
        }
        let g = metering::histogram_grid(&name, tuned);
        self.histogram = Some((name, g.clone()));
        g
    }

    fn metering_weights(&mut self, stats: &Statistics, meta: &FrameMetadata) -> (bool, Vec<f64>) {
        let name = meta
            .controls
            .metering_mode
            .clone()
            .unwrap_or_else(|| self.tuning.default_metering_mode.clone());
        let use_luma = stats
            .luma
            .as_ref()
            .is_some_and(|l| l.is_valid() && !l.is_empty());
        let (w, h) = match (&stats.luma, use_luma) {
            (Some(l), true) => (l.width, l.height),
            _ => (stats.colour.width, stats.colour.height),
        };
        if let Some((n, cw, ch, cl, weights)) = &self.weights
            && *n == name
            && (*cw, *ch, *cl) == (w, h, use_luma)
        {
            return (use_luma, weights.clone());
        }
        let key = if name == "average" && !self.tuning.metering_modes.contains_key("average") {
            "matrix"
        } else {
            name.as_str()
        };
        let weights = metering::weights_for(key, self.tuning.metering_modes.get(key), w, h);
        self.weights = Some((name, w, h, use_luma, weights.clone()));
        (use_luma, weights)
    }

    /// The gain needed relative to this frame, and the luma target in effect.
    fn compute_gain(
        &mut self,
        stats: &Statistics,
        meta: &FrameMetadata,
        p: &Params,
    ) -> (f64, f64, f64) {
        let lux = meta.lux.unwrap_or(p.lux);
        let ev_gain = 2f64.powf(meta.controls.ev) * self.tuning.base_ev;
        let mut target_y =
            (self.tuning.y_target.eval_clamped(lux) * ev_gain).min(EV_GAIN_Y_TARGET_LIMIT);
        let (use_luma, weights) = self.metering_weights(stats, meta);
        let measured = metering::weighted_y(stats, &weights, use_luma, p.colour_gains, 1.0);
        let mut gain = 1.0;
        for _ in 0..8 {
            let y = metering::weighted_y(stats, &weights, use_luma, p.colour_gains, gain);
            let extra = (target_y / (y + 0.001)).min(10.0);
            gain *= extra;
            if extra < 1.01 {
                break;
            }
        }
        let constraints = meta
            .controls
            .constraint_mode
            .as_ref()
            .and_then(|n| self.tuning.constraint_modes.get(n))
            .unwrap_or(&self.tuning.constraint_modes[&self.tuning.default_constraint_mode]);
        let h = &stats.histogram;
        if h.total() > 0 {
            for c in constraints {
                let ty = (c.y_target.eval_clamped(lux) * ev_gain).min(EV_GAIN_Y_TARGET_LIMIT);
                let iqm = h.inter_quantile_mean(c.q_lo, c.q_hi).max(1e-9);
                let g = ty * h.len() as f64 / iqm;
                if (c.bound == Bound::Lower && g > gain) || (c.bound == Bound::Upper && g < gain) {
                    gain = g;
                    target_y = ty;
                }
            }
        }
        (gain, target_y, measured)
    }

    /// Split a total exposure (without digital gain) along the exposure profile, snap to the
    /// flicker period and work out the digital gain for `total`.
    fn divide(
        &self,
        total_no_dg: f64,
        total: f64,
        fixed: (Option<f64>, Option<f64>),
        meta: Option<&FrameMetadata>,
    ) -> Split {
        let defaults = Controls::default();
        let controls = meta.map_or(&defaults, |m| &m.controls);
        let profile = self.profile(controls);
        let stage_t = |i: usize| self.limit_exposure(profile.exposure_us[i] * 1e-6, meta);
        let mut t = fixed.0.unwrap_or_else(|| stage_t(0));
        let mut g = fixed.1.unwrap_or_else(|| self.limit_gain(profile.gain[0]));
        if t * g < total_no_dg {
            for stage in 1..profile.gain.len() {
                if fixed.0.is_none() {
                    let st = stage_t(stage);
                    if st * g >= total_no_dg {
                        t = total_no_dg / g;
                        break;
                    }
                    t = st;
                }
                if fixed.1.is_none() {
                    if profile.gain[stage] * t >= total_no_dg {
                        g = total_no_dg / t;
                        break;
                    }
                    g = self.limit_gain(profile.gain[stage]);
                }
            }
        }
        let periods = if self.periods.is_empty() {
            controls
                .flicker
                .period()
                .map(|p| p.as_secs_f64())
                .into_iter()
                .collect()
        } else {
            self.periods.clone()
        };
        if fixed.0.is_none()
            && fixed.1.is_none()
            && controls.flicker != Flicker::Off
            && let Some(&period) = periods.iter().find(|&&p| t >= p)
        {
            let n = (t / period).floor();
            if n >= 1.0 {
                let snapped = n * period;
                g *= t / snapped;
                t = snapped;
            }
        }
        let analogue_gain = g.min(self.config.analogue_gain_limits.1);
        let no_dg = analogue_gain * t;
        let digital_gain = (total / no_dg).clamp(1.0, self.tuning.max_digital_gain);
        Split {
            exposure: t,
            analogue_gain,
            digital_gain,
        }
    }

    /// The start-up exposure and analogue gain: the warm start's, else the tuning's.
    fn start_values(&self) -> (f64, f64) {
        if let Some(w) = self.warm {
            return w;
        }
        let exposure = self.limit_exposure(self.tuning.default_exposure_us * 1e-6, None);
        let gain = self
            .limit_gain(self.tuning.default_analogue_gain)
            .min(self.config.analogue_gain_limits.1);
        (exposure, gain)
    }

    /// The start-up request (frame 0): what later requests are compared with.
    fn set_start_request(&mut self) {
        let (exposure, gain) = self.start_values();
        let request = SensorRequest {
            frame: 0,
            exposure: Duration::from_secs_f64(exposure),
            analogue_gain: gain,
            frame_duration: self.frame_duration_for(exposure, None),
        };
        self.last = Some((request.exposure, gain));
        self.last_request = Some(request);
    }

    fn frame_duration_for(&self, exposure: f64, meta: Option<&FrameMetadata>) -> Duration {
        let (fd_lo, fd_hi) = self.frame_duration_limits(meta);
        let fd = (exposure + self.config.exposure_margin.as_secs_f64()).clamp(fd_lo, fd_hi);
        Duration::from_secs_f64(fd)
    }
}

impl Algorithm for Agc {
    fn name(&self) -> &'static str {
        "agc"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        self.config = config.clone();
        self.weights = None;
        self.histogram = None;
        self.frame_count = 0;
        self.frozen = None;
        self.last = None;
        self.last_request = None;
        self.lands_at = 0;
        self.settles_at = 0;
        self.full_step_pending = false;
        self.lock_count = 0;
        self.warm = None;
        self.clock = None;
        self.fits.clear();
        self.detect_since.clear();
        self.detected = None;
        self.periods.clear();
        self.mains_period_seen = None;
        let (t, g) = self.start_values();
        self.filtered = t * g;
        self.set_start_request();
        Ok(())
    }

    fn warm_start(&mut self, warm: &WarmStart) {
        // The same scene through this mode: scale by the modes' sensitivities, then split along
        // the profile within this mode's limits, as the first frame's processing will (a
        // same-mode restart gets the split it ended with).
        let sensitivity = warm.sensitivity / self.config.sensitivity;
        let total = warm.total_exposure * sensitivity;
        if !(total.is_finite() && total > 0.0) {
            return;
        }
        self.detected = warm
            .flicker_detected
            .filter(|p| !p.is_zero())
            .map(|p| 0.5 / p.as_secs_f64());
        let s = self.divide(total, total, (None, None), None);
        self.warm = Some((s.exposure, s.analogue_gain));
        self.filtered = total;
        self.set_start_request();
    }

    fn initial(&self, params: &mut Params) {
        let (exposure, gain) = self.start_values();
        params.sensor = self.last_request;
        params.digital_gain =
            (self.filtered / (exposure * gain)).clamp(1.0, self.tuning.max_digital_gain);
        params.ae.total_exposure = self.filtered;
        params.ae.target_exposure = self.filtered;
        let (name, tuned) = self.metering_mode(None);
        params.histogram_weights = Some(metering::histogram_grid(&name, tuned));
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        self.frame_count = self.frame_count.saturating_add(1);
        let fixed = self.fixed(meta);
        let fixed_both = fixed.0.is_some() && fixed.1.is_some();
        params.histogram_weights = Some(self.histogram_weights(meta));
        let (gain, target_y, measured_y) = self.compute_gain(stats, meta, params);
        // Frames whose levels have not settled say nothing about the scene (see below).
        let unsettled = meta.frame < u64::from(self.config.unsettled_frames);
        // Meter against the mean light: a frame the flicker made brighter needs more gain.
        let (_, flicker_k) = self.flicker(stats, meta, !unsettled);
        let gain = gain * flicker_k;
        let on_target = (gain - 1.0).abs() < ON_TARGET;

        // Target total exposure, from what produced this frame: a frame exposed before a
        // change landed gives the same target as the frame that asked for it.
        let current =
            meta.exposure.as_secs_f64() * meta.analogue_gain * meta.digital_gain.max(1e-9);
        // The total exposures AE can reach, and whether the scene wants one beyond them.
        let mut reach = (0.0, f64::INFINITY);
        let target = if fixed_both {
            fixed.0.unwrap_or(0.0) * fixed.1.unwrap_or(0.0)
        } else {
            let profile = self.profile(&meta.controls);
            let max_t = fixed.0.unwrap_or_else(|| {
                self.limit_exposure(
                    profile.exposure_us[profile.exposure_us.len() - 1] * 1e-6,
                    Some(meta),
                )
            });
            let max_g = fixed
                .1
                .unwrap_or_else(|| self.limit_gain(profile.gain[profile.gain.len() - 1]));
            let min_t = fixed.0.unwrap_or(self.exposure_limits(Some(meta)).0);
            let min_g = fixed.1.unwrap_or(self.config.analogue_gain_limits.0);
            reach = (min_t * min_g, max_t * max_g);
            (current * gain).min(reach.1)
        };
        let want = current * gain;
        let beyond = want > reach.1 * (1.0 + ON_TARGET) || want < reach.0 * (1.0 - ON_TARGET);

        // Fast de-saturation: a saturated image under-states how far exposure must fall, so cut
        // the sensor exposure by `fast_reduce_threshold` at once (undamped), keeping image
        // brightness with digital gain meanwhile. Styx addition: only while at least half the
        // image is saturated; below that the metered gain is reliable and the extra cut would
        // undershoot.
        let h = &stats.histogram;
        let mostly_saturated = h.total() == 0 || h.quantile(0.5) >= 0.95 * h.len() as f64;
        let desaturating = self.tuning.desaturate
            && mostly_saturated
            && target_y > self.tuning.fast_reduce_threshold
            && gain < target_y.sqrt();

        // Damping only for small changes: a large one goes straight to the target (the delays
        // say when it lands, and frames before that ask for the same target again).
        let before = self.filtered;
        let large = self.tuning.full_step > 0.0
            && before > 0.0
            && (target / before - 1.0).abs() > self.tuning.full_step;
        // The first frame produced by an undamped change: what is left is the model's error
        // (clipped zones, black level), not noise, so it is corrected at once as well.
        let produced_by_last = self.last.is_some_and(|(t, g)| {
            let r =
                meta.exposure.as_secs_f64() * meta.analogue_gain / (t.as_secs_f64() * g).max(1e-12);
            (r - 1.0).abs() < 0.02
        });
        let correcting = self.full_step_pending && meta.frame >= self.lands_at && produced_by_last;
        if meta.frame >= self.lands_at {
            self.full_step_pending = false;
        }
        let (mut speed, mut stable) = (self.tuning.speed, self.tuning.stable_region);
        if fixed_both
            || self.frame_count <= self.tuning.startup_frames
            || desaturating
            || large
            || correcting
        {
            speed = 1.0;
        }
        // While a change is in flight, frames produced before it repeat what asked for it:
        // only a clearly different answer (the scene changed) replaces it.
        if meta.frame < self.lands_at {
            stable = stable.max(self.tuning.full_step);
        }
        // Frames whose levels have not settled say nothing about the scene; for as many frames
        // after them, and while locked, AE leaves a frame that is on target alone (no hunting
        // within the lock tolerance).
        if unsettled {
            stable = f64::INFINITY;
        } else if self.lock_count >= LOCK_FRAMES
            || meta.frame < 2 * u64::from(self.config.unsettled_frames)
        {
            stable = stable.max(ON_TARGET);
        }
        let stable = if fixed_both { 0.0 } else { stable };
        if before == 0.0 {
            self.filtered = target;
        } else if !(before * (1.0 - stable) < target && before * (1.0 + stable) > target) {
            if speed < 1.0 && before < 1.2 * target && before > 0.8 * target {
                speed = speed.sqrt();
            }
            self.filtered = speed * target + (1.0 - speed) * before;
        }
        let total = self.filtered;
        let no_dg = if desaturating {
            total * self.tuning.fast_reduce_threshold
        } else {
            total
        };
        let split = self.divide(no_dg, total, fixed, Some(meta));
        // Track what the sensor will do: after de-saturating, continue from the reduced
        // exposure rather than from the digitally compensated total.
        let dg = if desaturating {
            1.0
        } else {
            split.digital_gain
        };
        self.filtered = split.exposure * split.analogue_gain * dg;

        let exposure = Duration::from_secs_f64(split.exposure);
        let frame_duration = self.frame_duration_for(split.exposure, Some(meta));
        // Unchanged values repeat the last request as it was (nothing new to send).
        let request = match self.last_request {
            Some(r)
                if (r.exposure, r.analogue_gain, r.frame_duration)
                    == (exposure, split.analogue_gain, frame_duration) =>
            {
                r
            }
            last => {
                let frame = self.config.delays.earliest_landing(meta.frame);
                self.lands_at = frame;
                // A change within the lock tolerance leaves a frame on target either way.
                let total = |e: Duration, g: f64| e.as_secs_f64() * g;
                let significant = last.is_none_or(|r| {
                    let before = total(r.exposure, r.analogue_gain);
                    let now = total(exposure, split.analogue_gain);
                    (now / before.max(1e-12) - 1.0).abs() > ON_TARGET
                });
                if significant {
                    self.settles_at = frame;
                }
                self.full_step_pending = speed >= 1.0 && !fixed_both;
                SensorRequest {
                    frame,
                    exposure,
                    analogue_gain: split.analogue_gain,
                    frame_duration,
                }
            }
        };
        // At its limits: the scene wants more (or less) than AE can give and AE asks for the
        // limit; such a frame counts as on target (libcamera reports it converged too).
        let at_limit = beyond
            && [reach.0, reach.1]
                .iter()
                .any(|l| (self.filtered / l - 1.0).abs() < ON_TARGET);
        // Locked: produced with what AE asked for, on target (or at its limits), for
        // LOCK_FRAMES frames in a row.
        let settled =
            meta.frame >= self.settles_at && (on_target || at_limit) && !desaturating && !unsettled;
        self.lock_count = if settled {
            (self.lock_count + 1).min(LOCK_FRAMES)
        } else {
            0
        };
        self.last = Some((exposure, split.analogue_gain));
        self.last_request = Some(request);
        params.sensor = Some(request);
        params.digital_gain = split.digital_gain;
        params.ae = AeStatus {
            locked: self.lock_count >= LOCK_FRAMES,
            target_exposure: target,
            total_exposure: total,
            target_y,
            measured_y,
            desaturating,
            flicker_period: self.periods.first().map(|&p| Duration::from_secs_f64(p)),
            flicker_detected: self.detected.map(|hz| Duration::from_secs_f64(0.5 / hz)),
            flicker_modulation: flicker_k - 1.0,
            at_limit,
        };
    }
}

#[cfg(test)]
mod tests;
