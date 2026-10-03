//! AF's scans: the programmed contrast scan (coarse, fine, the approach), the parabola
//! through the peak, the PDAF loop step and the early end by phase (from Raspberry Pi's
//! `af.cpp`, BSD-2-Clause, Copyright (C) 2022-2023 Raspberry Pi Ltd; see the parent module).

use super::{Af, AfMode, AfState, LensState, Record, Scan};

impl Af {
    pub(super) fn start_programmed_scan(&mut self) {
        let (r, s) = (self.range_t(), self.speed_t());
        if !self.initted
            || self.mode != AfMode::Continuous
            || self.fsmooth <= r.min + 2.0 * s.step_coarse
        {
            self.ftarget = r.min;
            self.scan_step = s.step_coarse;
            self.scan = Scan::Coarse2;
        } else if self.fsmooth >= r.max - 2.0 * s.step_coarse {
            self.ftarget = r.max;
            self.scan_step = -s.step_coarse;
            self.scan = Scan::Coarse2;
        } else {
            self.scan_step = -s.step_coarse;
            self.scan = Scan::Coarse1;
        }
        self.scan_max_contrast = 0.0;
        self.scan_min_contrast = f64::INFINITY;
        self.scan_max_index = 0;
        self.scan_data.clear();
        self.step_count = s.step_frames;
        self.waiting_for_lens = true;
        self.waited = 0;
        self.report = AfState::Scanning;
    }

    pub(super) fn do_pdaf(&mut self, mut phase: f64, conf: f64) {
        let s = self.speed_t();
        phase *= s.pdaf_gain;
        if self.mode == AfMode::Continuous {
            // Scale down small or low-confidence moves, against wobble.
            phase *= conf / (conf + self.tuning.conf_epsilon);
            if phase.abs() < s.pdaf_squelch {
                let a = phase / s.pdaf_squelch;
                phase *= a * a;
            }
        } else if self.step_count >= s.step_frames {
            // Triggered: end early once the phase is small.
            if phase.abs() < s.pdaf_squelch {
                self.step_count = s.step_frames;
            }
        } else {
            // Smaller moves towards the end of the sequence, for a stable image.
            phase *= f64::from(self.step_count) / f64::from(s.step_frames.max(1));
        }
        let r = self.range_t();
        if phase < -s.max_slew {
            phase = -s.max_slew;
            self.report = if self.ftarget <= r.min {
                AfState::Failed
            } else {
                AfState::Scanning
            };
        } else if phase > s.max_slew {
            phase = s.max_slew;
            self.report = if self.ftarget >= r.max {
                AfState::Failed
            } else {
                AfState::Scanning
            };
        } else {
            self.report = AfState::Focused;
        }
        self.ftarget = self.fsmooth + phase;
    }

    pub(super) fn early_termination_by_phase(&mut self, phase: f64) -> bool {
        let Some(last) = self.scan_data.last().copied() else {
            return false;
        };
        if last.conf < self.tuning.conf_thresh {
            return false;
        }
        // Gradient finite with the expected sign: interpolate the zero-phase position.
        if (self.ftarget - last.focus) * (phase - last.phase) * self.speed_t().pdaf_gain < 0.0 {
            let param = phase / (phase - last.phase);
            if (-2.5 <= param || self.mode == AfMode::Continuous) && param <= 3.0 {
                let param = param.max(-2.5);
                self.ftarget += param * (last.focus - self.ftarget);
                return true;
            }
        }
        false
    }

    /// The parabola through the peak sample and its neighbours.
    pub(super) fn find_peak(&self, mut i: usize) -> f64 {
        let d = &self.scan_data;
        let mut f = d[i].focus;
        if d.len() >= 3 {
            if i == 0 {
                i += 1;
            } else if i + 1 >= d.len() {
                i -= 1;
            }
            let abx = d[i - 1].focus - d[i].focus;
            let aby = d[i - 1].contrast - d[i].contrast;
            let cbx = d[i + 1].focus - d[i].focus;
            let cby = d[i + 1].contrast - d[i].contrast;
            let denom = 2.0 * (aby * cbx - cby * abx);
            // libcamera's |denom| ≥ 1/64 in its units: relative to the contrast here.
            let scale = self.scan_max_contrast.abs().max(f64::MIN_POSITIVE);
            if denom.abs() >= 1e-6 * scale * abx.abs() * cbx.abs() && denom * abx > 0.0 {
                let p = (aby * cbx * cbx - cby * abx * abx) / denom;
                f = p.clamp(abx.min(cbx), abx.max(cbx)) + d[i].focus;
            }
        }
        f
    }

    pub(super) fn do_scan(&mut self, contrast: f64, phase: f64, conf: f64) {
        let (r, s) = (self.range_t(), self.speed_t());
        if self.scan_data.is_empty() || contrast > self.scan_max_contrast {
            self.scan_max_contrast = contrast;
            self.scan_max_index = self.scan_data.len();
            if self.scan != Scan::Fine {
                self.old_scene_average = self.prev_average;
            }
        }
        self.scan_min_contrast = self.scan_min_contrast.min(contrast);
        self.scan_data.push(Record {
            focus: self.ftarget,
            contrast,
            phase,
            conf,
        });
        if (self.scan_step >= 0.0 && self.ftarget >= r.max)
            || (self.scan_step <= 0.0 && self.ftarget <= r.min)
            || (self.scan == Scan::Fine && self.scan_data.len() >= 3)
            || (contrast < s.contrast_ratio * self.scan_max_contrast
                && self.scan_max_contrast - contrast > 3.0 * self.noise)
        {
            let pk = self.find_peak(self.scan_max_index);
            // A first coarse scan that did not bracket the peak reverses; a fine scan (or a
            // coarse one without fine steps) ends; else a fine scan back over the peak.
            if self.scan == Scan::Coarse1
                && self.scan_data[0].contrast >= s.contrast_ratio * self.scan_max_contrast
            {
                self.scan_step = -self.scan_step;
                self.scan = Scan::Coarse2;
            } else if self.scan == Scan::Fine || s.step_fine <= 0.0 {
                // Styx: the samples were taken moving in the scan's direction, so a lens with
                // backlash sat on that side of each; reach the peak moving the same way.
                let d = self.scan_step.signum();
                if (pk - self.ftarget) * d < 0.0 && s.step_fine > 0.0 {
                    self.approach = pk;
                    self.ftarget = (pk - d * s.step_fine).clamp(r.min, r.max);
                    self.scan = Scan::Approach;
                } else {
                    self.ftarget = pk;
                    self.scan = Scan::Settle;
                }
            } else if self.scan_step >= 0.0 {
                // Styx: a peak at the near or far end still gets three fine samples.
                self.ftarget = (pk + s.step_fine).max(r.min + 2.0 * s.step_fine).min(r.max);
                self.scan_step = -s.step_fine;
                self.scan = Scan::Fine;
            } else {
                self.ftarget = (pk - s.step_fine).min(r.max - 2.0 * s.step_fine).max(r.min);
                self.scan_step = s.step_fine;
                self.scan = Scan::Fine;
            }
            self.scan_data.clear();
        } else {
            self.ftarget += self.scan_step;
        }
        self.step_count = if self.ftarget == self.fsmooth {
            0
        } else {
            s.step_frames
        };
        self.waiting_for_lens = self.ftarget != self.fsmooth;
        self.waited = 0;
    }

    /// Whether the scan may measure this frame: the lens settled at the target in the frame's
    /// report (frame-exact), or `step_frames` counted down (no report).
    pub(super) fn step_ready(&mut self, lens: Option<LensState>) -> bool {
        match lens.filter(|_| self.tuning.frame_exact) {
            Some(l) => {
                let at = (l.position - f64::from(self.code(self.fsmooth))).abs() <= 0.5;
                let limit = 3 * self.speed_t().step_frames + 5;
                if self.waiting_for_lens && !(l.settled && at) && self.waited < limit {
                    self.waited += 1;
                    return false;
                }
                self.waiting_for_lens = false;
                self.waited = 0;
                self.step_count = 0;
                true
            }
            None => {
                if self.step_count > 0 {
                    self.step_count -= 1;
                    false
                } else {
                    true
                }
            }
        }
    }
}
