//! A focus lens and a scene with depth for the simulator: the frames' focus statistics
//! (contrast per zone) and phase-detection data follow the lens's real position, which
//! follows the moves AF asks for with a settle time, hysteresis and noise.
//!
//! * [`FocusScene`]: a subject at a distance (dioptres, piecewise linear over frames) in a
//!   region of the image, a background at another, and the subject's texture (contrast).
//! * [`LensModel`]: the voice-coil motor. A move written at time `t_w` from `p0` to `c`
//!   follows `c' + (p0 - c') e^{-(t - t_w)/τ}` with `τ = settle / 5`, where `c'` stops short of
//!   `c` by half the hysteresis (backlash, in the direction of travel); its true dioptre →
//!   position map may differ from the tuning's; position noise per frame. The lens control's
//!   report ([`crate::LensState`]) is the nominal model: the commanded position once `settle`
//!   has passed, the exponential before (what `styx-sensor`'s lens model predicts).
//! * [`Optics`]: blur against defocus `δ` (dioptres) as `1 / (1 + (δ / depth_of_focus)²)`,
//!   averaged over four instants of the exposure (a lens moving during the exposure blurs);
//!   a zone's figure of merit is `texture × L² × blur`, `L` its mean level, plus the noise
//!   the ISP's noise floor leaves (`2σ²` of the pixel noise times `floor_noise`, Gaussian; `σ²`
//!   from the sensor model's shot and read noise taken as before the analogue gain) and
//!   a relative error (`fom_noise`). Low light (small `L`, large `σ²/L²`) or no texture makes
//!   the curve flat and noisy.
//! * [`PdafModel`]: per cell, phase = defocus / the sensor's dioptres per phase unit, with
//!   noise that grows as confidence falls; confidence falls with defocus and with texture ×
//!   signal-to-noise.

use alloc::collections::BTreeMap;
use alloc::{vec, vec::Vec};
use core::time::Duration;

use super::Rng;
use crate::algos::af::{AfWindow, LensRequest, LensState};
#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use crate::pwl::Pwl;
use crate::stats::{PdafZone, Statistics, ZoneGrid};

/// The scene's depth.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusScene {
    /// The subject's distance in dioptres by frame number.
    pub subject: Pwl,
    /// The background's distance in dioptres.
    pub background: f64,
    /// Where the subject is (fractions of the image).
    pub region: AfWindow,
    /// Texture: gradient energy relative to the squared level, by frame (0: a blank wall).
    pub texture: Pwl,
}

impl FocusScene {
    /// A subject at `dioptres` filling the default AF area, against a background at infinity.
    pub fn subject_at(dioptres: f64) -> Self {
        Self {
            subject: Pwl::constant(dioptres),
            background: 0.0,
            region: AfWindow::new(0.2, 0.25, 0.6, 0.5),
            texture: Pwl::constant(0.05),
        }
    }
}

/// The lens and its motor.
#[derive(Debug, Clone, PartialEq)]
pub struct LensModel {
    /// Driver positions.
    pub range: (i32, i32),
    /// The true dioptre → position relation.
    pub map: Pwl,
    /// Time a move takes to settle (to within a code).
    pub settle: Duration,
    /// Backlash in codes: the lens stops short by half of it in the direction of travel.
    pub hysteresis: f64,
    /// Position noise per frame (codes, standard deviation).
    pub noise: f64,
    /// Frames from a move's write to the first frame it is for ([`crate::LensConfig::delay`]).
    pub delay: u32,
}

impl Default for LensModel {
    /// The IMX708 module's map (Raspberry Pi's `rpi.af` default), a 10-bit VCM settling in
    /// 12 ms, 4 codes of hysteresis, 0.5 codes of noise.
    fn default() -> Self {
        Self {
            range: (0, 1023),
            map: Pwl::new(vec![(0.0, 445.0), (15.0, 925.0)]).expect("valid"),
            settle: Duration::from_millis(12),
            hysteresis: 4.0,
            noise: 0.5,
            delay: 2,
        }
    }
}

/// Phase detection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdafModel {
    /// Cells (16×12 on the IMX708).
    pub grid: (u32, u32),
    /// The sensor's dioptres per phase unit (IMX708: about −0.016, the tuning's `pdaf_gain`).
    pub dioptres_per_unit: f64,
    /// Confidence of a well-textured cell in focus at full signal.
    pub confidence: f64,
    /// Defocus (dioptres) at which confidence halves.
    pub range: f64,
    /// Phase noise (units) at confidence 100; grows as `sqrt(100 / conf)`.
    pub noise: f64,
}

impl Default for PdafModel {
    fn default() -> Self {
        Self {
            grid: (16, 12),
            dioptres_per_unit: -0.016,
            confidence: 400.0,
            range: 3.0,
            noise: 4.0,
        }
    }
}

/// How focus shows in the statistics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Optics {
    /// Focus statistics grid (8×8 on the PiSP).
    pub grid: (u32, u32),
    /// Defocus (dioptres) at which the figure of merit halves.
    pub depth_of_focus: f64,
    /// Relative error of the figure of merit per frame.
    pub fom_noise: f64,
    /// Noise the ISP's noise floor leaves, relative to `2σ²` of the pixel noise.
    pub floor_noise: f64,
    /// Phase detection, if the sensor has it.
    pub pdaf: Option<PdafModel>,
}

impl Default for Optics {
    fn default() -> Self {
        Self {
            grid: (8, 8),
            depth_of_focus: 0.6,
            fom_noise: 0.02,
            floor_noise: 0.3,
            pdaf: None,
        }
    }
}

/// One lens move: written at `at` (seconds) towards `target` from `from`.
#[derive(Debug, Clone, Copy)]
struct Move {
    at: f64,
    from: f64,
    target: f64,
    /// Where the lens really stops (hysteresis).
    stop: f64,
    /// Nominal start, for the report.
    from_nominal: f64,
}

/// The focus part of a [`super::Simulation`]: set `Simulation::focus` to use it.
#[derive(Debug, Clone)]
pub struct FocusSim {
    /// The scene's depth.
    pub scene: FocusScene,
    /// The lens.
    pub lens: LensModel,
    /// The optics and statistics.
    pub optics: Optics,
    /// Report lens positions with the frames (`FrameMetadata::lens`; off: AF counts frames).
    pub report: bool,
    rng: Rng,
    current: Move,
    /// Writes waiting for their frame start: write frame → position.
    pending: BTreeMap<u64, i32>,
    sent: Option<i32>,
    /// True lens position (dioptres) at the middle of each simulated frame's exposure.
    pub history: Vec<f64>,
    /// Moves written.
    pub moves: u64,
}

impl FocusSim {
    /// A lens resting at its lowest position.
    pub fn new(scene: FocusScene, lens: LensModel, optics: Optics) -> Self {
        let p = f64::from(lens.range.0);
        Self {
            scene,
            lens,
            optics,
            report: true,
            rng: Rng::new(0xF0C5),
            current: Move {
                at: -1.0,
                from: p,
                target: p,
                stop: p,
                from_nominal: p,
            },
            pending: BTreeMap::new(),
            sent: None,
            history: Vec::new(),
            moves: 0,
        }
    }

    /// The lens configuration AF should be given (`CameraConfig::lens`), without a map (AF's
    /// tuning has one).
    pub fn lens_config(&self) -> crate::LensConfig {
        crate::LensConfig {
            range: self.lens.range,
            delay: self.lens.delay,
            map: None,
        }
    }

    fn tau(&self) -> f64 {
        (self.lens.settle.as_secs_f64() / 5.0).max(1e-6)
    }

    /// True position at `t`.
    fn position(&self, t: f64) -> f64 {
        let m = &self.current;
        let dt = (t - m.at).max(0.0);
        m.stop + (m.from - m.stop) * (-dt / self.tau()).exp()
    }

    /// The nominal (reported) position at `t`.
    fn nominal(&self, t: f64) -> (f64, bool) {
        let m = &self.current;
        let dt = t - m.at;
        if dt >= self.lens.settle.as_secs_f64() {
            (m.target, true)
        } else {
            let dt = dt.max(0.0);
            (
                m.target + (m.from_nominal - m.target) * (-dt / self.tau()).exp(),
                false,
            )
        }
    }

    /// Writes a move at time `t`.
    fn write(&mut self, t: f64, position: i32) {
        let (lo, hi) = self.lens.range;
        let target = f64::from(position.clamp(lo, hi));
        let from = self.position(t);
        let (from_nominal, _) = self.nominal(t);
        let dir = (target - from).signum();
        let stop = target - dir * self.lens.hysteresis / 2.0;
        self.current = Move {
            at: t,
            from,
            target,
            stop,
            from_nominal,
        };
        self.moves += 1;
    }

    /// Frame `frame` starts at time `t`: writes what is due.
    pub(super) fn frame_start(&mut self, frame: u64, t: f64) {
        let due: Vec<u64> = self.pending.range(..=frame).map(|(k, _)| *k).collect();
        if let Some(&last) = due.last() {
            let p = self.pending[&last];
            for k in due {
                self.pending.remove(&k);
            }
            self.write(t, p);
        }
    }

    /// Handles AF's request made while processing frame `frame` (at time `now`, the start of
    /// the next frame): written at once when it is due, else at the start of its frame.
    pub(super) fn request(&mut self, r: &LensRequest, frame: u64, latency: u64, now: f64) {
        if self.sent == Some(r.position) {
            return;
        }
        self.sent = Some(r.position);
        let earliest = frame + latency;
        let write = r
            .frame
            .saturating_sub(u64::from(self.lens.delay))
            .max(earliest);
        if latency == 0 && write <= frame + 1 {
            self.write(now + 0.001, r.position);
        } else {
            self.pending.insert(write, r.position);
        }
    }

    /// Sets the lens's starting position (as the pipeline's start-up request does).
    pub fn start_at(&mut self, position: i32) {
        let p = f64::from(position);
        self.current = Move {
            at: -1.0,
            from: p,
            target: p,
            stop: p,
            from_nominal: p,
        };
        self.sent = Some(position);
    }

    /// Focus statistics and phase data for frame `frame`, exposed over `[t0, t0 + exposure]`,
    /// into `stats`; returns the lens report.
    pub(super) fn observe(
        &mut self,
        frame: u64,
        t0: f64,
        exposure: f64,
        noise: (f64, f64),
        gain: f64,
        stats: &mut Statistics,
    ) -> Option<LensState> {
        let f = frame as f64;
        let subject = self.scene.subject.eval_clamped(f);
        let texture = self.scene.texture.eval_clamped(f).max(0.0);
        let inv = self.lens.map.inverse();
        let to_d = |p: f64| inv.as_ref().map_or(0.0, |m| m.eval(p));
        let jitter = self.lens.noise * self.rng.normal();
        let samples: Vec<f64> = (0..4)
            .map(|i| to_d(self.position(t0 + exposure * (f64::from(i) + 0.5) / 4.0) + jitter))
            .collect();
        self.history.push(samples.iter().sum::<f64>() / 4.0);
        let region = self.scene.region;
        let inside = |gx: u32, gy: u32, gw: u32, gh: u32| {
            let cx = (f64::from(gx) + 0.5) / f64::from(gw);
            let cy = (f64::from(gy) + 0.5) / f64::from(gh);
            cx >= region.x
                && cx < region.x + region.width
                && cy >= region.y
                && cy < region.y + region.height
        };
        let snapshot = stats.clone();
        let level = |gx: u32, gy: u32, gw: u32, gh: u32| luma_at(&snapshot, gx, gy, gw, gh);
        // Noise before the gain: shot noise variance and read noise scale with it.
        let (shot, read) = (noise.0 * gain, noise.1 * gain);
        let o = self.optics;
        let (fw, fh) = o.grid;
        let mut focus = ZoneGrid::<f64>::new(fw, fh);
        for gy in 0..fh {
            for gx in 0..fw {
                let depth = if inside(gx, gy, fw, fh) {
                    subject
                } else {
                    self.scene.background
                };
                let blur = samples
                    .iter()
                    .map(|d| 1.0 / (1.0 + ((depth - d) / o.depth_of_focus).powi(2)))
                    .sum::<f64>()
                    / 4.0;
                let l = level(gx, gy, fw, fh);
                let var = shot * l + read * read;
                let signal = texture * l * l * blur;
                let v = signal * (1.0 + o.fom_noise * self.rng.normal())
                    + 2.0 * var * o.floor_noise * self.rng.normal();
                focus.zones[(gy * fw + gx) as usize] = v.max(0.0);
            }
        }
        stats.focus = Some(focus);
        if let Some(p) = o.pdaf {
            let (pw, ph) = p.grid;
            let mut grid = ZoneGrid::<PdafZone>::new(pw, ph);
            let mid = samples[1].midpoint(samples[2]);
            for gy in 0..ph {
                for gx in 0..pw {
                    let depth = if inside(gx, gy, pw, ph) {
                        subject
                    } else {
                        self.scene.background
                    };
                    let l = level(gx, gy, pw, ph);
                    let var = shot * l + read * read;
                    let snr = (texture * l * l / (2.0 * var).max(1e-30)).min(1e6);
                    let defocus = depth - mid;
                    let conf =
                        p.confidence * snr / (snr + 1.0) / (1.0 + (defocus / p.range).powi(2));
                    let conf = conf.floor().clamp(0.0, 2047.0);
                    let phase = if conf > 0.0 {
                        let sd = p.noise * (100.0 / conf).sqrt();
                        (defocus / p.dioptres_per_unit + sd * self.rng.normal()).round()
                    } else {
                        0.0
                    };
                    grid.zones[(gy * pw + gx) as usize] = PdafZone {
                        phase: phase.clamp(-2048.0, 2047.0),
                        conf,
                    };
                }
            }
            stats.pdaf = Some(grid);
        }
        let (position, settled_end) = self.nominal(t0 + exposure);
        let (_, settled_start) = self.nominal(t0);
        let (mid_position, _) = self.nominal(t0 + exposure / 2.0);
        self.report.then_some(LensState {
            position: if settled_end && settled_start {
                position
            } else {
                mid_position
            },
            settled: settled_start,
        })
    }
}

/// Mean luma of the statistics' luma (or colour) zone under focus cell `(gx, gy)` of a
/// `gw × gh` grid.
fn luma_at(stats: &Statistics, gx: u32, gy: u32, gw: u32, gh: u32) -> f64 {
    if let Some(l) = stats
        .luma
        .as_ref()
        .filter(|l| l.is_valid() && !l.is_empty())
    {
        let x = ((f64::from(gx) + 0.5) / f64::from(gw) * f64::from(l.width)) as u32;
        let y = ((f64::from(gy) + 0.5) / f64::from(gh) * f64::from(l.height)) as u32;
        let z = l.zones[(y.min(l.height - 1) * l.width + x.min(l.width - 1)) as usize];
        if z.counted > 0 {
            return z.y / f64::from(z.counted);
        }
    }
    stats.mean_luma()
}
