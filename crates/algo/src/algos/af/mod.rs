//! Autofocus: a hybrid of phase-detection (PDAF) and contrast (CDAF) AF driving a lens
//! (a voice-coil motor) in dioptres.
//!
//! Ported from Raspberry Pi's `controller/rpi/af.cpp` (BSD-2-Clause, Copyright (C) 2022-2023
//! Raspberry Pi Ltd). The method:
//!
//! * PDAF, whenever the sensor sends phase data with enough confidence: a feedback loop moves
//!   the lens by `phase × pdaf_gain` per frame (slew limited). Triggered (auto mode) it runs
//!   `pdaf_frames` frames, ending early once the phase is small; in continuous mode it runs
//!   all the time, its small moves squashed against wobble.
//! * Otherwise CDAF: a coarse scan in `step_coarse` steps (from the near or far end, or from
//!   the current position in both directions in continuous mode) that stops when contrast
//!   falls below `contrast_ratio` of its peak, a parabola through the peak and its
//!   neighbours, a fine scan back over the peak in `step_fine` steps, another parabola. The
//!   result is *focused* when the peak stands out (the lowest contrast seen is below
//!   `contrast_ratio` of it, and the final position still has that much), else *failed*. A
//!   scan ends early when two PDAF samples let the zero-phase position be interpolated.
//! * Continuous mode without PDAF: a scan after each scene change (contrast or the windows'
//!   colour moving by more than `retrigger_ratio`) once it has been stable for
//!   `retrigger_delay` frames. Phase data dropping out for `dropout_frames` frames falls back
//!   to the same.
//! * Windows: up to 10 rectangles with weights; the middle half × third by default. Contrast
//!   is the weighted mean of the focus statistics' zones, phase the confidence-weighted mean.
//!
//! Styx changes:
//!
//! * Lens moves are [`LensRequest`]s naming the frame they are for, applied by the lens
//!   control frame-exactly (as exposure is), and every frame reports where the lens was
//!   ([`crate::FrameMetadata::lens`], predicted from the moves and the lens's move time). A
//!   scan step is measured on the first frame exposed with the lens settled at it, instead of
//!   `step_frames` later (`frame_exact`; without reports it counts `step_frames` as libcamera).
//! * Contrast is this frame's (libcamera's is the previous frame's: its PDAF runs before the
//!   frame's statistics are in), over the windows' squared green level, so it does not move
//!   with exposure (AE settling during a scan, flicker).
//! * Scene-change tests are relative (libcamera adds 1 in its fixed-point units).
//! * The contrast's noise is followed (frame-to-frame change with the lens still, filtered).
//!   A coarse scan stops at a drop only if the drop is more than three times the noise (far
//!   from focus the curve is flat and noisy, and libcamera's scan stops there by chance); a
//!   scan is reported focused only if the noise is below half of `1 - contrast_ratio` of the
//!   peak (in noise a flat curve passes libcamera's test by chance).
//! * A failed scan moves the lens to the range's default (hyperfocal) position instead of the
//!   best of a flat or noisy curve, and until a scan succeeds again continuous AF retriggers on
//!   colour or brightness changes only (contrast that is mostly noise would retrigger it
//!   forever).
//! * The lens reaches the peak moving the way the fine scan did (over one fine step when the
//!   peak lies behind it): a VCM with backlash stops short in the direction it moves, so the
//!   peak measured moving one way is where the lens must arrive moving that way.
//! * A peak at an end of the range still gets three fine samples (libcamera's fine scan then
//!   runs off the end with two).
//! * Controls arrive with each frame ([`crate::Controls`]): a trigger or cancel is a counter
//!   that changed, so recordings replay them. Switching to manual applies the lens position
//!   control at once.
//! * Pausing continuous AF is not implemented yet.

use alloc::{vec, vec::Vec};

mod measure;
mod scan;
#[cfg(test)]
mod tests;
pub mod tuning;
mod types;

use crate::config::{CameraConfig, LensConfig};
use crate::error::Result;
use crate::frame::{Controls, FrameMetadata};
#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use crate::params::Params;
use crate::pipeline::Algorithm;
use crate::pwl::Pwl;
use crate::stats::Statistics;

pub use measure::MAX_WINDOWS;
use measure::{Measure, Measurement};
pub use tuning::{AfRangeTuning, AfRanges, AfSpeedTuning, AfSpeeds, AfTuning};
pub use types::{AfMode, AfRange, AfSpeed, AfState, AfStatus, AfWindow, LensRequest, LensState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scan {
    Idle,
    Trigger,
    Pdaf,
    Coarse1,
    Coarse2,
    Fine,
    /// Styx: on the way to the peak from the side the fine scan measured it from.
    Approach,
    Settle,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    focus: f64,
    contrast: f64,
    phase: f64,
    conf: f64,
}

/// The AF algorithm. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Af {
    tuning: AfTuning,
    lens: Option<LensConfig>,
    /// Dioptres → driver position in use.
    map: Pwl,
    issue_latency: u64,
    measure: Measure,
    // Controls.
    mode: AfMode,
    range: AfRange,
    speed: AfSpeed,
    seen_trigger: u32,
    seen_cancel: u32,
    seen_lens_position: Option<f64>,
    // Working state, as af.cpp.
    scan: Scan,
    initted: bool,
    ir: bool,
    ftarget: f64,
    fsmooth: f64,
    prev_contrast: f64,
    old_scene_contrast: f64,
    prev_average: [f64; 3],
    old_scene_average: [f64; 3],
    prev_phase: f64,
    skip_count: u32,
    step_count: u32,
    drop_count: u32,
    same_sign_count: u32,
    scene_change_count: u32,
    scan_max_index: usize,
    scan_max_contrast: f64,
    scan_min_contrast: f64,
    scan_step: f64,
    scan_data: Vec<Record>,
    report: AfState,
    // Styx.
    /// The frame-exact wait: measure once a frame shows the lens settled at the target.
    waiting_for_lens: bool,
    /// Frames waited for the lens (a lens that never reports settled is given up on).
    waited: u32,
    scan_frames: u32,
    last_request: Option<LensRequest>,
    /// Frame-to-frame change of contrast with the lens still (filtered): the noise of the
    /// figure of merit, mostly what the ISP's noise floor leaves.
    noise: f64,
    /// The previous frame's contrast and lens position, for `noise`.
    still: Option<(f64, f64)>,
    /// Frames the commanded position has not moved (without lens reports).
    still_frames: u32,
    /// The peak an approach goes to.
    approach: f64,
}

impl Af {
    /// AF with this tuning.
    pub fn new(tuning: AfTuning) -> Result<Self> {
        tuning.validate()?;
        Ok(Self {
            tuning,
            lens: None,
            map: Pwl::default(),
            issue_latency: 0,
            measure: Measure::default(),
            mode: AfMode::Manual,
            range: AfRange::Normal,
            speed: AfSpeed::Normal,
            seen_trigger: 0,
            seen_cancel: 0,
            seen_lens_position: None,
            scan: Scan::Idle,
            initted: false,
            ir: false,
            ftarget: -1.0,
            fsmooth: -1.0,
            prev_contrast: 0.0,
            old_scene_contrast: 0.0,
            prev_average: [0.0; 3],
            old_scene_average: [0.0; 3],
            prev_phase: 0.0,
            skip_count: 0,
            step_count: 0,
            drop_count: 0,
            same_sign_count: 0,
            scene_change_count: 0,
            scan_max_index: 0,
            scan_max_contrast: 0.0,
            scan_min_contrast: f64::INFINITY,
            scan_step: 0.0,
            scan_data: Vec::new(),
            report: AfState::Idle,
            waiting_for_lens: false,
            waited: 0,
            scan_frames: 0,
            last_request: None,
            noise: 0.0,
            still: None,
            still_frames: 0,
            approach: 0.0,
        })
    }

    /// The dioptre → lens driver map for a lens: the tuning's, else the lens's own, else a
    /// straight line from the normal range's far end at the lens's lowest position to its
    /// near end at the highest.
    pub fn lens_map(tuning: &AfTuning, lens: &LensConfig) -> Pwl {
        if !tuning.map.is_empty() {
            return tuning.map.clone();
        }
        if let Some(m) = lens.map.as_ref().filter(|m| m.points().len() >= 2) {
            return m.clone();
        }
        let n = tuning.ranges.normal;
        let (lo, hi) = (f64::from(lens.range.0), f64::from(lens.range.1));
        let (d0, d1) = if n.max > n.min {
            (n.min, n.max)
        } else {
            (0.0, 12.0)
        };
        Pwl::new(vec![(d0, lo), (d1, hi)]).unwrap_or_else(|_| Pwl::constant(lo))
    }

    /// The lens limits in dioptres (the map's domain).
    pub fn lens_limits(&self) -> (f64, f64) {
        self.map.domain()
    }

    fn range_t(&self) -> AfRangeTuning {
        self.tuning.ranges.get(self.range)
    }

    fn speed_t(&self) -> AfSpeedTuning {
        self.tuning.speeds.get(self.speed)
    }

    fn code(&self, dioptres: f64) -> i32 {
        let v = self.map.eval_clamped(dioptres).round();
        match &self.lens {
            Some(l) => (v as i32).clamp(l.range.0, l.range.1),
            None => v as i32,
        }
    }

    // ---- Controls ----

    fn set_mode(&mut self, mode: AfMode) {
        if self.mode != mode {
            self.mode = mode;
            if mode == AfMode::Continuous {
                self.scan = Scan::Trigger;
            } else if mode != AfMode::Auto || self.scan < Scan::Coarse1 {
                self.go_idle();
            }
        }
    }

    fn trigger(&mut self) {
        if self.mode == AfMode::Auto && self.scan == Scan::Idle {
            self.scan = Scan::Trigger;
        }
    }

    fn cancel(&mut self) {
        if self.mode == AfMode::Auto {
            self.go_idle();
        }
    }

    fn set_lens_position(&mut self, dioptres: f64) {
        if self.mode == AfMode::Manual {
            let (lo, hi) = self.map.domain();
            self.ftarget = dioptres.clamp(lo, hi);
            self.update_lens_position();
        }
    }

    fn apply_controls(&mut self, c: &Controls) {
        // A mode change first, so the other controls see the new mode (as libcamera).
        let to_manual = c.af_mode == AfMode::Manual && self.mode != AfMode::Manual;
        self.set_mode(c.af_mode);
        self.range = c.af_range;
        if self.speed != c.af_speed {
            let (old, new) = (self.speed_t(), self.tuning.speeds.get(c.af_speed));
            if self.scan == Scan::Pdaf && new.pdaf_frames > old.pdaf_frames {
                self.step_count += new.pdaf_frames - old.pdaf_frames;
            }
            self.speed = c.af_speed;
        }
        self.measure.set_windows(&c.af_windows);
        if c.af_cancel != self.seen_cancel {
            self.seen_cancel = c.af_cancel;
            self.cancel();
        }
        if c.af_trigger != self.seen_trigger {
            self.seen_trigger = c.af_trigger;
            self.trigger();
        }
        if c.lens_position != self.seen_lens_position || to_manual {
            self.seen_lens_position = c.lens_position;
            if let Some(d) = c.lens_position.filter(|d| d.is_finite()) {
                self.set_lens_position(d);
            }
        }
    }

    // ---- The core, as af.cpp ----

    fn go_idle(&mut self) {
        self.scan = Scan::Idle;
        self.report = AfState::Idle;
        self.scan_data.clear();
        self.waiting_for_lens = false;
    }

    fn update_lens_position(&mut self) {
        if self.scan >= Scan::Pdaf {
            let r = self.range_t();
            self.ftarget = self.ftarget.clamp(r.min, r.max);
        }
        if self.initted {
            let s = self.speed_t().max_slew;
            self.fsmooth = self.ftarget.clamp(self.fsmooth - s, self.fsmooth + s);
        } else {
            self.fsmooth = self.ftarget;
            self.initted = true;
            self.skip_count = self.tuning.skip_frames;
        }
    }

    fn start_af(&mut self) {
        let s = self.speed_t();
        if s.pdaf_gain != 0.0
            && s.dropout_frames > 0
            && (self.mode == AfMode::Continuous || s.pdaf_frames > 0)
        {
            if !self.initted {
                self.ftarget = self.range_t().default;
                self.update_lens_position();
            }
            self.step_count = if self.mode == AfMode::Continuous {
                0
            } else {
                s.pdaf_frames
            };
            self.scan = Scan::Pdaf;
            self.scan_data.clear();
            self.drop_count = 0;
            self.old_scene_contrast = 0.0;
            self.scene_change_count = 0;
            self.report = AfState::Scanning;
        } else {
            self.start_programmed_scan();
            self.update_lens_position();
        }
        self.scan_frames = 0;
    }

    fn changed(&self, a: f64, b: f64) -> bool {
        let k = self.speed_t().retrigger_ratio;
        let eps = 1e-9 * a.abs().max(b.abs()).max(1e-30);
        a + eps < k * b || b + eps < k * a
    }

    fn do_af(&mut self, contrast: f64, phase: f64, conf: f64, lens: Option<LensState>) {
        let contrast_counts = self.report != AfState::Failed;
        if self.skip_count > 0 {
            self.skip_count -= 1;
            return;
        }
        if phase * self.prev_phase <= 0.0 {
            self.same_sign_count = 0;
        } else {
            self.same_sign_count += 1;
        }
        self.prev_phase = phase;
        if self.mode == AfMode::Manual {
            return;
        }
        let s = self.speed_t();
        if self.scan == Scan::Pdaf {
            // PDAF closed loop while it has confidence; after `dropout_frames` without, a
            // contrast scan (triggered) or waiting for a scene change (continuous).
            if conf >= self.tuning.conf_epsilon {
                if self.mode == AfMode::Auto || self.same_sign_count >= 3 {
                    self.do_pdaf(phase, conf);
                }
                if self.step_count > 0 {
                    self.step_count -= 1;
                } else if self.mode != AfMode::Continuous {
                    self.scan = Scan::Idle;
                }
                self.old_scene_contrast = contrast;
                self.old_scene_average = self.prev_average;
                self.scene_change_count = 0;
                self.drop_count = 0;
                return;
            }
            self.drop_count += 1;
            if self.drop_count < s.dropout_frames {
                return;
            }
            if self.mode != AfMode::Continuous {
                self.start_programmed_scan();
                return;
            }
        }
        if self.scan < Scan::Coarse1 && self.mode == AfMode::Continuous {
            // Not scanning, no PDAF: wait for a scene change, then stability.
            let moved = (contrast_counts && self.changed(contrast, self.old_scene_contrast))
                || (0..3).any(|i| self.changed(self.prev_average[i], self.old_scene_average[i]));
            if moved {
                self.old_scene_contrast = contrast;
                self.old_scene_average = self.prev_average;
                self.scene_change_count = 1;
            } else if self.scene_change_count > 0 {
                self.scene_change_count += 1;
            }
            if self.scene_change_count >= s.retrigger_delay {
                self.start_programmed_scan();
            }
        } else if self.scan >= Scan::Coarse1 && self.fsmooth == self.ftarget {
            if !self.step_ready(lens) {
                return;
            }
            if self.scan == Scan::Settle {
                // Styx: the peak must also stand out of the contrast's own noise.
                let quiet = self.noise < (1.0 - s.contrast_ratio) / 2.0 * self.scan_max_contrast;
                self.report = if contrast >= s.contrast_ratio * self.scan_max_contrast
                    && self.scan_min_contrast <= s.contrast_ratio * self.scan_max_contrast
                    && quiet
                {
                    AfState::Focused
                } else {
                    AfState::Failed
                };
                if self.report == AfState::Failed {
                    self.ftarget = self.range_t().default;
                }
                self.scan = if self.mode == AfMode::Continuous && s.dropout_frames > 0 {
                    Scan::Pdaf
                } else {
                    Scan::Idle
                };
                self.drop_count = 0;
                self.scene_change_count = 0;
                self.old_scene_contrast = self.scan_max_contrast.max(contrast);
                self.scan_data.clear();
            } else if self.scan == Scan::Approach {
                self.ftarget = self.approach;
                self.scan = Scan::Settle;
                self.waiting_for_lens = self.ftarget != self.fsmooth;
                self.step_count = s.step_frames;
                self.waited = 0;
            } else if conf >= self.tuning.conf_thresh && self.early_termination_by_phase(phase) {
                self.old_scene_average = self.prev_average;
                self.scan = Scan::Settle;
                self.step_count = if self.mode == AfMode::Continuous {
                    0
                } else {
                    s.step_frames
                };
                self.waiting_for_lens = self.mode != AfMode::Continuous;
                self.waited = 0;
            } else {
                self.do_scan(contrast, phase, conf);
            }
        }
    }

    /// Follows how much contrast moves between frames while the lens stands still (a flat
    /// curve in noise, low light, passes libcamera's peak test by chance).
    ///
    /// The lens is still when the frame's report says it settled where the previous frame's
    /// was; without reports, when the commanded position has not moved for `step_frames`.
    fn track_noise(&mut self, lens: Option<LensState>) {
        let c = self.prev_contrast;
        let at = match lens {
            Some(l) => l.settled.then_some(l.position),
            None => {
                if self.still.is_none_or(|(_, p)| p != self.fsmooth) {
                    self.still_frames = 0;
                }
                self.still_frames = self.still_frames.saturating_add(1);
                (self.still_frames > self.speed_t().step_frames).then_some(self.fsmooth)
            }
        };
        if let (Some((prev, p)), Some(now)) = (self.still, at)
            && p == now
            && c.max(prev) > 0.0
        {
            self.noise += 0.2 * ((c - prev).abs() - self.noise);
        }
        self.still = match lens {
            Some(_) => at.map(|p| (c, p)),
            None => Some((c, self.fsmooth)),
        };
    }

    fn status(&self, m: &Measurement) -> AfStatus {
        let state = if self.mode == AfMode::Auto && self.scan != Scan::Idle {
            AfState::Scanning
        } else if self.mode == AfMode::Manual {
            AfState::Idle
        } else {
            self.report
        };
        AfStatus {
            active: true,
            mode: self.mode,
            state,
            lens_position: self.initted.then_some(self.fsmooth),
            contrast: m.contrast.unwrap_or(0.0),
            phase: m.phase,
            confidence: m.conf,
            contrast_noise: self.noise,
            scan_frames: if state == AfState::Scanning {
                self.scan_frames
            } else {
                0
            },
        }
    }

    fn request(&mut self, frame: u64, params: &mut Params) {
        let Some(lens) = &self.lens else { return };
        let position = self.code(self.fsmooth);
        if self.last_request.is_some_and(|r| r.position == position) {
            // Nothing new: keep the request (and the frame it named).
            params.lens = self.last_request;
            return;
        }
        let r = LensRequest {
            frame: frame + self.issue_latency + u64::from(lens.delay),
            position,
            dioptres: self.fsmooth,
        };
        self.last_request = Some(r);
        params.lens = Some(r);
    }
}

impl Algorithm for Af {
    fn name(&self) -> &'static str {
        "af"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        self.lens = config.lens.clone();
        self.issue_latency = u64::from(config.delays.issue_latency);
        let map = self
            .lens
            .as_ref()
            .map(|l| Self::lens_map(&self.tuning, l))
            .unwrap_or_default();
        if map != self.map {
            // Another lens: start from its default position.
            self.map = map;
            self.initted = false;
        }
        if self.scan >= Scan::Coarse1 && self.scan < Scan::Settle {
            // A scan in progress restarts (the statistics may have changed).
            self.start_programmed_scan();
        }
        if self.lens.is_some() && !self.initted {
            self.ftarget = self.tuning.ranges.normal.default;
            self.update_lens_position();
        }
        self.measure = Measure::default();
        self.still = None;
        self.skip_count = self.tuning.skip_frames;
        self.last_request = None;
        self.waiting_for_lens = self.scan >= Scan::Coarse1;
        self.waited = 0;
        Ok(())
    }

    fn initial(&self, params: &mut Params) {
        if self.lens.is_none() {
            return;
        }
        params.lens = Some(LensRequest {
            frame: 0,
            position: self.code(self.fsmooth),
            dioptres: self.fsmooth,
        });
        params.af = self.status(&Measurement::default());
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        if self.lens.is_none() {
            params.af = AfStatus::default();
            params.lens = None;
            return;
        }
        self.apply_controls(&meta.controls);
        if self.scan == Scan::Trigger {
            self.start_af();
        }
        let t = &self.tuning;
        let m = self
            .measure
            .measure(stats, t.conf_thresh, t.conf_clip, t.check_for_ir);
        self.prev_contrast = m.contrast.unwrap_or(0.0);
        self.prev_average = m.rgb;
        self.ir = m.ir;
        self.track_noise(meta.lens);
        if self.initted {
            let conf = if self.ir { 0.0 } else { m.conf };
            self.do_af(self.prev_contrast, m.phase, conf, meta.lens);
            self.update_lens_position();
        }
        if self.scan != Scan::Idle {
            self.scan_frames = self.scan_frames.saturating_add(1);
        }
        params.af = self.status(&m);
        self.request(meta.frame, params);
    }
}
