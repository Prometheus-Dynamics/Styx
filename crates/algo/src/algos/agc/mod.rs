//! AGC / AE: automatic exposure and gain.
//!
//! Ported from Raspberry Pi's `agc_channel.cpp` (BSD-2-Clause, Copyright (C) 2023 Raspberry Pi
//! Ltd), channel 0 only, with these differences:
//!
//! * Output carries the frame it applies from ([`crate::SensorRequest::frame`]), computed from
//!   the sensor's control delays, instead of relying on a separate delayed-controls helper.
//! * The frame duration is chosen here (exposure + margin, within the limits).
//! * EV is in stops; zone weights come from the tuning or built-in shapes (see `metering`).
//! * Undamped changes (start-up, fixed values, de-saturation) are held until the frame they
//!   land on, so frames still in flight do not re-trigger them.
//! * De-saturation happens only while at least half the image is saturated, without damping,
//!   and continues from the reduced sensor exposure (the original re-derives it from the
//!   digitally compensated total, which overshoots with multi-frame delays).
//! * Locked also requires the current frame to be within 5% of the target.
//!
//! Per frame: meter the scene (weighted zone luma, iterated because saturated zones do not
//! scale), apply the histogram constraints, compute the target total exposure relative to what
//! actually produced this frame, damp it, split it into exposure time / analogue gain / digital
//! gain along the exposure profile, and snap the exposure time to the flicker period.

mod metering;
pub mod tuning;

use std::time::Duration;

use crate::config::CameraConfig;
use crate::error::Result;
use crate::frame::FrameMetadata;
use crate::params::{AeStatus, Params, SensorRequest};
use crate::pipeline::Algorithm;
use crate::stats::Statistics;

use tuning::{AgcTuning, Bound, ExposureProfile};

/// Luma targets are capped here: histograms cannot be read near saturation.
const EV_GAIN_Y_TARGET_LIMIT: f64 = 0.9;
/// Frames of consistent exposure needed to report a lock.
const MAX_LOCK_COUNT: u32 = 5;

/// The AGC algorithm. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Agc {
    tuning: AgcTuning,
    config: CameraConfig,
    weights: Option<(String, u32, u32, bool, Vec<f64>)>,
    frame_count: u32,
    /// Damped total exposure (seconds × gain), with and without digital gain.
    filtered: f64,
    /// Values AE was frozen at when switched off.
    frozen: Option<(Duration, f64)>,
    last: Option<(Duration, f64)>,
    last_target: f64,
    lock_count: u32,
    /// An undamped change lands on this frame; hold until then.
    hold_until: Option<u64>,
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
            frame_count: 0,
            filtered: 0.0,
            frozen: None,
            last: None,
            last_target: 0.0,
            lock_count: 0,
            hold_until: None,
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

    fn profile(&self, meta: &FrameMetadata) -> &ExposureProfile {
        meta.controls
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
        meta: &FrameMetadata,
    ) -> Split {
        let profile = self.profile(meta);
        let stage_t = |i: usize| self.limit_exposure(profile.exposure_us[i] * 1e-6, Some(meta));
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
        if fixed.0.is_none()
            && fixed.1.is_none()
            && let Some(period) = meta.controls.flicker.period()
        {
            let period = period.as_secs_f64();
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

    fn update_lock(&mut self, meta: &FrameMetadata, target: f64) -> bool {
        const ERR: f64 = 0.10;
        const RESET: f64 = 1.5;
        let (t, g) = (meta.exposure.as_secs_f64(), meta.analogue_gain);
        let (lt, lg) = self
            .last
            .map(|(t, g)| (t.as_secs_f64(), g))
            .unwrap_or((t, g));
        let (et, eg, ex) = (lt * ERR + 200e-6, lg * ERR, self.last_target * ERR);
        let within = |m: f64| {
            (t - lt).abs() < m * et
                && (g - lg).abs() < m * eg
                && (target - self.last_target).abs() < m * ex
        };
        if within(1.0) {
            self.lock_count = (self.lock_count + 1).min(MAX_LOCK_COUNT);
        } else if !within(RESET) {
            self.lock_count = 0;
        }
        self.last_target = target;
        self.lock_count == MAX_LOCK_COUNT
    }
}

impl Algorithm for Agc {
    fn name(&self) -> &'static str {
        "agc"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        self.config = config.clone();
        self.weights = None;
        self.frame_count = 0;
        self.frozen = None;
        self.last = None;
        self.last_target = 0.0;
        self.lock_count = 0;
        self.hold_until = None;
        let t = self.limit_exposure(self.tuning.default_exposure_us * 1e-6, None);
        self.filtered = t * self.limit_gain(self.tuning.default_analogue_gain);
        Ok(())
    }

    fn initial(&self, params: &mut Params) {
        let exposure = self.limit_exposure(self.tuning.default_exposure_us * 1e-6, None);
        let gain = self
            .limit_gain(self.tuning.default_analogue_gain)
            .min(self.config.analogue_gain_limits.1);
        let (fd_lo, fd_hi) = self.frame_duration_limits(None);
        let fd = (exposure + self.config.exposure_margin.as_secs_f64()).clamp(fd_lo, fd_hi);
        params.sensor = Some(SensorRequest {
            frame: 0,
            exposure: Duration::from_secs_f64(exposure),
            analogue_gain: gain,
            frame_duration: Duration::from_secs_f64(fd),
        });
        params.digital_gain = 1.0;
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        self.frame_count = self.frame_count.saturating_add(1);
        let fixed = self.fixed(meta);
        let (gain, target_y, measured_y) = self.compute_gain(stats, meta, params);
        let on_target = (gain - 1.0).abs() < 0.05;

        // An undamped change is still on its way: this frame says nothing new about it, so
        // hold the request until the frame it lands on (the frame-exact delays tell which).
        if self.hold_until.is_some_and(|f| meta.frame < f) {
            let target = self.last_target;
            params.ae.measured_y = measured_y;
            params.ae.locked = self.update_lock(meta, target) && on_target;
            return;
        }
        self.hold_until = None;

        let current =
            meta.exposure.as_secs_f64() * meta.analogue_gain * meta.digital_gain.max(1e-9);
        // Target total exposure.
        let target = match fixed {
            (Some(t), Some(g)) => t * g,
            _ => {
                let profile = self.profile(meta);
                let max_t = fixed.0.unwrap_or_else(|| {
                    self.limit_exposure(
                        profile.exposure_us[profile.exposure_us.len() - 1] * 1e-6,
                        Some(meta),
                    )
                });
                let max_g = fixed
                    .1
                    .unwrap_or_else(|| self.limit_gain(profile.gain[profile.gain.len() - 1]));
                (current * gain).min(max_t * max_g)
            }
        };

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

        // Damping. Styx addition: none while de-saturating (see above).
        let (mut speed, mut stable) = (self.tuning.speed, self.tuning.stable_region);
        if (fixed.0.is_some() && fixed.1.is_some())
            || self.frame_count <= self.tuning.startup_frames
            || desaturating
        {
            speed = 1.0;
            stable = 0.0;
        }
        let before = self.filtered;
        if before == 0.0 {
            self.filtered = target;
        } else if !(before * (1.0 - stable) < target && before * (1.0 + stable) > target) {
            if before < 1.2 * target && before > 0.8 * target {
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
        let split = self.divide(no_dg, total, fixed, meta);
        // Track what the sensor will do: after de-saturating, continue from the reduced
        // exposure rather than from the digitally compensated total.
        let dg = if desaturating {
            1.0
        } else {
            split.digital_gain
        };
        self.filtered = split.exposure * split.analogue_gain * dg;

        let landing = self.config.delays.earliest_landing(meta.frame);
        if speed >= 1.0 && (self.filtered - before).abs() > 1e-3 * before.max(1e-12) {
            self.hold_until = Some(landing);
        }
        // Locked: exposure steady for several frames and this frame already on target.
        let locked = self.update_lock(meta, if desaturating { 0.0 } else { target }) && on_target;
        let exposure = Duration::from_secs_f64(split.exposure);
        self.last = Some((exposure, split.analogue_gain));
        let (fd_lo, fd_hi) = self.frame_duration_limits(Some(meta));
        let fd = (split.exposure + self.config.exposure_margin.as_secs_f64()).clamp(fd_lo, fd_hi);
        params.sensor = Some(SensorRequest {
            frame: landing,
            exposure,
            analogue_gain: split.analogue_gain,
            frame_duration: Duration::from_secs_f64(fd),
        });
        params.digital_gain = split.digital_gain;
        params.ae = AeStatus {
            locked,
            target_exposure: target,
            total_exposure: total,
            target_y,
            measured_y,
            desaturating,
        };
    }
}

#[cfg(test)]
mod tests;
