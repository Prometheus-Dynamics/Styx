//! ALSC: lens-shading tables interpolated by colour temperature and refined from the colour
//! statistics.
//!
//! Ported from Raspberry Pi's `alsc.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi Ltd):
//! calibration interpolation (`getCalTable`), resampling to the mode's crop and flips
//! (`resampleCalTable`), normalising (`compensateLambdasForCal`), the luminance table and its
//! strength (`addLuminanceToTables`, `generateLut`), and the adaptive refinement (`doAlsc`,
//! in [`adaptive`]): every `frame_period` frames (every frame for `startup_frames`) the red and
//! blue gains are re-estimated from the statistics; the tables move towards each new result
//! with `speed` per frame (at once during start-up).
//!
//! The original runs the refinement on its own thread and picks the result up a frame or more
//! later; here it runs in `process` and its result is used from the next `process` on, so the
//! output is a function of the inputs (replays stay bit-identical). It needs the statistics on
//! the tables' grid (the PiSP's 32x32 AWB zones); on other grids the tables follow the
//! calibration only, as with `n_iter = 0`.

// The tuning types are always there; the algorithm needs feature `alsc` (on with `std`).
#![cfg_attr(not(feature = "alsc"), allow(dead_code, unused_imports))]

use alloc::{format, string::String, vec, vec::Vec};

#[cfg(feature = "alsc")]
mod adaptive;

use serde::{Deserialize, Serialize};

use crate::config::{CameraConfig, Crop};
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use crate::params::{LensShading, Params};
use crate::pipeline::Algorithm;
use crate::stats::Statistics;
use crate::warm::WarmStart;

/// A calibrated colour table at a temperature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlscCalibration {
    /// Colour temperature (K).
    pub ct: f64,
    /// Row-major gains over the grid.
    pub table: Vec<f64>,
}

/// ALSC tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AlscTuning {
    /// Grid (width, height) of every table.
    pub grid: (u32, u32),
    /// Red (Cr) tables in increasing temperature.
    pub calibrations_cr: Vec<AlscCalibration>,
    /// Blue (Cb) tables in increasing temperature.
    pub calibrations_cb: Vec<AlscCalibration>,
    /// Luminance gains over the grid; empty for unity (or generated from `corner_strength`).
    pub luminance_lut: Vec<f64>,
    /// Generate the luminance table from a cos⁴-like fall-off with this corner gain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corner_strength: Option<f64>,
    /// Horizontal stretch of the generated fall-off.
    pub asymmetry: f64,
    /// How much of the luminance correction to apply.
    pub luminance_strength: f64,
    /// Temperature used before AWB has an estimate.
    pub default_ct: f64,
    /// Frames between adaptive runs once started.
    pub frame_period: u32,
    /// Frames from the start during which the adaptive part runs every frame and its results
    /// are taken at once.
    pub startup_frames: u32,
    /// How far the tables move towards a new result per frame (0..1).
    pub speed: f64,
    /// Colour similarity scale of the red ratios: neighbours this different get weight e^-½.
    pub sigma_cr: f64,
    /// The same for blue.
    pub sigma_cb: f64,
    /// Zones with no more pixels than this are not used.
    pub min_count: f64,
    /// Zones whose mean green (or red, or blue) is no more than this are not used.
    pub min_g: f64,
    /// Over-relaxation factor of the iterations.
    pub omega: f64,
    /// Iterations per run; `None`: the grid's width plus height; 0 turns the adaptive part off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n_iter: Option<u32>,
    /// Iterations stop once no gain changes by more than this.
    pub threshold: f64,
    /// The adaptive gains stay within `1 ± lambda_bound`.
    pub lambda_bound: f64,
}

impl Default for AlscTuning {
    fn default() -> Self {
        Self {
            grid: (16, 12),
            calibrations_cr: Vec::new(),
            calibrations_cb: Vec::new(),
            luminance_lut: Vec::new(),
            corner_strength: None,
            asymmetry: 1.0,
            luminance_strength: 1.0,
            default_ct: 4500.0,
            frame_period: 12,
            startup_frames: 10,
            speed: 0.05,
            sigma_cr: 0.01,
            sigma_cb: 0.01,
            min_count: 10.0,
            min_g: 50.0 / 65536.0,
            omega: 1.3,
            n_iter: None,
            threshold: 1e-3,
            lambda_bound: 0.05,
        }
    }
}

impl AlscTuning {
    fn cells(&self) -> usize {
        (self.grid.0 * self.grid.1) as usize
    }

    /// Check the tuning.
    pub fn validate(&self) -> Result<()> {
        let err = |m: String| Err(AlgoError::Tuning(format!("alsc: {m}")));
        if self.grid.0 == 0 || self.grid.1 == 0 {
            return err("empty grid".into());
        }
        for (name, cals) in [("cr", &self.calibrations_cr), ("cb", &self.calibrations_cb)] {
            if cals.windows(2).any(|w| w[1].ct <= w[0].ct) {
                return err(format!("calibrations_{name} must be in increasing ct"));
            }
            if cals.iter().any(|c| c.table.len() != self.cells()) {
                return err(format!(
                    "calibrations_{name}: table size does not match grid"
                ));
            }
            if cals
                .iter()
                .any(|c| c.table.iter().any(|v| v.is_nan() || *v <= 0.0))
            {
                return err(format!("calibrations_{name}: gains must be positive"));
            }
        }
        if !self.luminance_lut.is_empty() && self.luminance_lut.len() != self.cells() {
            return err("luminance_lut size does not match grid".into());
        }
        if self.corner_strength.is_some_and(|c| c <= 1.0) || self.asymmetry < 0.0 {
            return err("corner_strength must be > 1 and asymmetry >= 0".into());
        }
        let positive = [self.sigma_cr, self.sigma_cb, self.omega, self.threshold];
        if positive.iter().any(|v| v.is_nan() || *v <= 0.0)
            || !(0.0..=1.0).contains(&self.speed)
            || !(0.0..1.0).contains(&self.lambda_bound)
            || self.frame_period == 0
        {
            return err(
                "sigma_cr, sigma_cb, omega, threshold must be > 0, speed in 0..=1, \
                 lambda_bound in 0..1, frame_period > 0"
                    .into(),
            );
        }
        Ok(())
    }

    /// Adaptive iterations per run.
    fn iterations(&self) -> u32 {
        self.n_iter.unwrap_or(self.grid.0 + self.grid.1)
    }

    /// The luminance table: generated (`corner_strength` wins, as in the original), tuned, or
    /// unity.
    fn luminance(&self) -> Vec<f64> {
        let (w, h) = (self.grid.0 as i64, self.grid.1 as i64);
        let Some(cs) = self.corner_strength else {
            if !self.luminance_lut.is_empty() {
                return self.luminance_lut.clone();
            }
            return vec![1.0; self.cells()];
        };
        let (f1, f2) = (cs - 1.0, 1.0 + cs.sqrt());
        let r2max = (w * h / 4) as f64 * (1.0 + self.asymmetry * self.asymmetry);
        let mut out = Vec::with_capacity(self.cells());
        for y in 0..h {
            for x in 0..w {
                let dy = (y - h / 2) as f64 + 0.5;
                let dx = ((x - w / 2) as f64 + 0.5) * self.asymmetry;
                let r2 = (dx * dx + dy * dy) / r2max;
                out.push((f1 * r2 + f2) * (f1 * r2 + f2) / (f2 * f2));
            }
        }
        out
    }
}

#[cfg(feature = "alsc")]
/// Interpolate the calibrations at a temperature (unity when there are none).
fn cal_table(cals: &[AlscCalibration], ct: f64, n: usize) -> Vec<f64> {
    match cals {
        [] => vec![1.0; n],
        [first, ..] if ct <= first.ct => first.table.clone(),
        [.., last] if ct >= last.ct => last.table.clone(),
        _ => {
            let i = cals.windows(2).position(|w| w[1].ct >= ct).unwrap_or(0);
            let (a, b) = (&cals[i], &cals[i + 1]);
            a.table
                .iter()
                .zip(&b.table)
                .map(|(x, y)| (x * (b.ct - ct) + y * (ct - a.ct)) / (b.ct - a.ct))
                .collect()
        }
    }
}

#[cfg(feature = "alsc")]
/// Bilinear resampling of a full-array table to the crop, with flips.
fn resample(t: &[f64], (w, h): (u32, u32), crop: Crop, hflip: bool, vflip: bool) -> Vec<f64> {
    let (wi, hi) = (w as i64, h as i64);
    let axis = |n: i64, off: f64, scale: f64, flip: bool| -> Vec<(usize, usize, f64)> {
        let mut v = 0.5 * scale + off * n as f64 - 0.5;
        (0..n)
            .map(|_| {
                let lo = v.floor() as i64;
                let f = v - lo as f64;
                let (mut a, mut b) = (lo.max(0).min(n - 1), (lo + 1).min(n - 1).max(0));
                if flip {
                    (a, b) = (n - 1 - a, n - 1 - b);
                }
                v += scale;
                (a as usize, b as usize, f)
            })
            .collect()
    };
    let xs = axis(wi, crop.x, crop.width, hflip);
    let ys = axis(hi, crop.y, crop.height, vflip);
    let mut out = Vec::with_capacity(t.len());
    for &(y0, y1, fy) in &ys {
        for &(x0, x1, fx) in &xs {
            let at = |y: usize, x: usize| t[y * w as usize + x];
            let above = at(y0, x0) * (1.0 - fx) + at(y0, x1) * fx;
            let below = at(y1, x0) * (1.0 - fx) + at(y1, x1) * fx;
            out.push(above * (1.0 - fy) + below * fy);
        }
    }
    out
}

#[cfg(feature = "alsc")]
/// Relative distance below which the filtered tables take the target.
const SNAP: f64 = 1e-3;

#[cfg(feature = "alsc")]
/// The ALSC algorithm.
#[derive(Debug, Clone)]
pub struct Alsc {
    tuning: AlscTuning,
    config: CameraConfig,
    luminance: Vec<f64>,
    /// Colour temperature of the latest run.
    ct: f64,
    /// Adaptive red and blue gains (without the calibration), kept across runs.
    lambda: [Vec<f64>; 2],
    /// The latest run's tables, not yet taken (`syncResults`).
    pending: Option<[Vec<f64>; 3]>,
    /// The latest result the filtered tables move towards.
    target: [Vec<f64>; 3],
    /// The tables in use (`prevSyncResults`).
    current: [Vec<f64>; 3],
    /// Frames since the start (up to `startup_frames`), since the last run, and the last frame.
    frame_count: u32,
    frame_phase: u32,
    last_frame: Option<u64>,
    /// Tables have been set up for this mode.
    started: bool,
    /// Iterations of the last adaptive run (red, blue).
    last_iterations: (u32, u32),
}

#[cfg(feature = "alsc")]
impl Alsc {
    /// An ALSC algorithm.
    pub fn new(tuning: AlscTuning) -> Result<Self> {
        tuning.validate()?;
        Ok(Self {
            ct: tuning.default_ct,
            tuning,
            config: CameraConfig::default(),
            luminance: Vec::new(),
            lambda: [Vec::new(), Vec::new()],
            pending: None,
            target: [Vec::new(), Vec::new(), Vec::new()],
            current: [Vec::new(), Vec::new(), Vec::new()],
            frame_count: 0,
            frame_phase: 0,
            last_frame: None,
            started: false,
            last_iterations: (0, 0),
        })
    }

    /// The calibrated red and blue tables for a temperature, resampled to the mode.
    fn calibration(&self, ct: f64) -> [Vec<f64>; 2] {
        let t = &self.tuning;
        let c = &self.config;
        let n = t.cells();
        [&t.calibrations_cr, &t.calibrations_cb]
            .map(|cals| resample(&cal_table(cals, ct, n), t.grid, c.crop, c.hflip, c.vflip))
    }

    /// Final tables from adaptive gains and calibrated tables: their product normalised to a
    /// smallest gain of 1 (`compensateLambdasForCal`), times the luminance table at its
    /// strength (`addLuminanceToTables`).
    fn combine(&self, lambda: &[Vec<f64>; 2], cal: &[Vec<f64>; 2]) -> [Vec<f64>; 3] {
        let t = &self.tuning;
        let lum: Vec<f64> = self
            .luminance
            .iter()
            .map(|l| (l - 1.0) * t.luminance_strength + 1.0)
            .collect();
        let colour = |k: usize| {
            let v: Vec<f64> = lambda[k].iter().zip(&cal[k]).map(|(l, c)| l * c).collect();
            let min = v.iter().copied().fold(f64::INFINITY, f64::min);
            v.iter().zip(&lum).map(|(x, l)| x / min * l).collect()
        };
        [colour(0), lum.clone(), colour(1)]
    }

    /// Tables for a colour temperature from the calibration alone (no adaptive gains).
    pub fn tables(&self, ct: f64) -> LensShading {
        let n = self.tuning.cells();
        let ones = [vec![1.0; n], vec![1.0; n]];
        self.shading(&self.combine(&ones, &self.calibration(ct)))
    }

    /// Iterations the last adaptive run took (red, blue); (0, 0) before the first.
    pub fn last_iterations(&self) -> (u32, u32) {
        self.last_iterations
    }

    fn shading(&self, [r, g, b]: &[Vec<f64>; 3]) -> LensShading {
        LensShading {
            width: self.tuning.grid.0,
            height: self.tuning.grid.1,
            r: r.clone(),
            g: g.clone(),
            b: b.clone(),
        }
    }

    /// A fresh start at the current temperature (`switchMode` with a table reset).
    fn reset_tables(&mut self) {
        let n = self.tuning.cells();
        self.lambda = [vec![1.0; n], vec![1.0; n]];
        let tables = self.combine(&self.lambda, &self.calibration(self.ct));
        self.target.clone_from(&tables);
        self.current = tables;
        self.pending = None;
    }

    /// One adaptive run (`doAlsc`) on a frame's statistics: the next tables.
    fn run(&mut self, stats: &Statistics) -> [Vec<f64>; 3] {
        let cal = self.calibration(self.ct);
        let t = &self.tuning;
        let applied = (!stats.before_lsc).then(|| {
            let [r, g, b] = &self.current;
            [r.as_slice(), g.as_slice(), b.as_slice()]
        });
        let settings = adaptive::Settings {
            sigma_cr: t.sigma_cr,
            sigma_cb: t.sigma_cb,
            min_count: t.min_count,
            min_g: t.min_g,
            omega: t.omega,
            n_iter: t.iterations(),
            threshold: t.threshold,
            lambda_bound: t.lambda_bound,
        };
        if t.iterations() > 0
            && let Some(zones) = adaptive::zones_of(&stats.colour, t.grid, applied)
        {
            let [lr, lb] = &mut self.lambda;
            self.last_iterations =
                adaptive::run(&zones, t.grid, (&cal[0], &cal[1]), (lr, lb), &settings);
        }
        self.combine(&self.lambda, &cal)
    }
}

#[cfg(feature = "alsc")]
impl Algorithm for Alsc {
    fn name(&self) -> &'static str {
        "alsc"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        // Keep the adaptive gains when the mode crops and flips the same (the original resets
        // them only for a "significant" change).
        let same = self.started
            && config.crop == self.config.crop
            && (config.hflip, config.vflip) == (self.config.hflip, self.config.vflip);
        self.config = config.clone();
        let c = &self.config;
        self.luminance = resample(
            &self.tuning.luminance(),
            self.tuning.grid,
            c.crop,
            c.hflip,
            c.vflip,
        );
        if same {
            let tables = self.combine(&self.lambda, &self.calibration(self.ct));
            self.target.clone_from(&tables);
            self.current = tables;
            self.pending = None;
        } else {
            self.reset_tables();
        }
        self.started = true;
        self.frame_count = 0;
        // Run again as soon as there are statistics.
        self.frame_phase = self.tuning.frame_period;
        self.last_frame = None;
        Ok(())
    }

    fn warm_start(&mut self, warm: &WarmStart) {
        if warm.colour_temperature > 0.0 && (warm.colour_temperature - self.ct).abs() > 1e-9 {
            self.ct = warm.colour_temperature;
            let tables = self.combine(&self.lambda, &self.calibration(self.ct));
            self.target.clone_from(&tables);
            self.current = tables;
        }
    }

    fn initial(&self, params: &mut Params) {
        params.lens_shading = Some(self.shading(&self.current));
    }

    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params) {
        let t = &self.tuning;
        // Frames since the last call: the original filters on every frame, the pipeline may
        // run the algorithms on fewer.
        let frames = self
            .last_frame
            .map_or(1, |f| meta.frame.saturating_sub(f).clamp(1, 1000) as u32);
        self.last_frame = Some(meta.frame);
        self.frame_count = (self.frame_count + frames).min(t.startup_frames);
        let startup = self.frame_count < t.startup_frames;
        // `prepare`: take the last run's result and move the tables towards it.
        if let Some(next) = self.pending.take() {
            self.target = next;
        }
        let keep = if startup {
            0.0
        } else {
            (1.0 - t.speed).powi(frames as i32)
        };
        // Within SNAP of the target the tables take it as is, so they stop changing (an ISP
        // re-packs its tables only when they change).
        let close = self
            .current
            .iter()
            .flatten()
            .zip(self.target.iter().flatten())
            .all(|(c, x)| (c - x).abs() <= SNAP * x.abs());
        for (cur, target) in self.current.iter_mut().zip(&self.target) {
            for (c, x) in cur.iter_mut().zip(target) {
                *c = if close {
                    *x
                } else {
                    (1.0 - keep) * x + keep * *c
                };
            }
        }
        params.lens_shading = Some(self.shading(&self.current));
        // `process`: start a run on this frame's statistics when one is due.
        self.frame_phase = (self.frame_phase + frames).min(t.frame_period);
        if self.frame_phase >= t.frame_period || startup {
            self.ct = params.colour_temperature;
            self.pending = Some(self.run(stats));
            self.frame_phase = 0;
        }
    }
}

#[cfg(all(test, feature = "alsc"))]
mod tests {
    use super::*;

    fn tuning() -> AlscTuning {
        AlscTuning {
            grid: (2, 2),
            calibrations_cr: vec![
                AlscCalibration {
                    ct: 3000.0,
                    table: vec![2.0, 2.0, 2.0, 4.0],
                },
                AlscCalibration {
                    ct: 5000.0,
                    table: vec![1.0, 1.0, 1.0, 3.0],
                },
            ],
            luminance_lut: vec![1.0, 1.0, 1.0, 2.0],
            luminance_strength: 0.5,
            ..Default::default()
        }
    }

    #[test]
    fn interpolates_normalises_and_adds_luminance() {
        let mut a = Alsc::new(tuning()).unwrap();
        a.prepare(&CameraConfig::default()).unwrap();
        let t = a.tables(4000.0);
        // Cr at 4000: [1.5, 1.5, 1.5, 3.5] / 1.5, then × (1 + (lut - 1) × 0.5).
        assert!((t.r[0] - 1.0).abs() < 1e-12);
        assert!((t.r[3] - 3.5 / 1.5 * 1.5).abs() < 1e-12);
        assert_eq!(t.g, vec![1.0, 1.0, 1.0, 1.5]);
        assert_eq!(t.b, t.g);
    }

    #[test]
    fn flips_mirror_the_tables() {
        let mut a = Alsc::new(tuning()).unwrap();
        a.prepare(&CameraConfig {
            hflip: true,
            vflip: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(a.tables(5000.0).r[0], 4.5);
    }

    #[test]
    fn generated_luminance_rises_to_the_corners() {
        let t = AlscTuning {
            grid: (8, 6),
            corner_strength: Some(2.0),
            ..Default::default()
        };
        let l = t.luminance();
        assert!(l[0] > l[3 * 8 + 4]);
        assert!(l.iter().all(|v| *v >= 1.0));
    }

    /// Statistics of a grey surface whose right half reads `k` times redder.
    fn stats(w: u32, h: u32, k: f64) -> Statistics {
        use crate::stats::{ColourZone, ZoneGrid};
        let zones = (0..w * h)
            .map(|i| {
                let r = if i % w >= w / 2 { 0.2 * k } else { 0.2 };
                ColourZone {
                    r: r * 1000.0,
                    g: 200.0,
                    b: 200.0,
                    counted: 1000,
                }
            })
            .collect();
        Statistics {
            colour: ZoneGrid {
                width: w,
                height: h,
                zones,
            },
            before_lsc: true,
            ..Default::default()
        }
    }

    #[test]
    fn adapts_during_start_up_then_every_period_with_speed() {
        let (w, h) = (8, 6);
        let mut a = Alsc::new(AlscTuning {
            grid: (w, h),
            sigma_cr: 0.05,
            frame_period: 4,
            startup_frames: 3,
            speed: 0.5,
            ..Default::default()
        })
        .unwrap();
        a.prepare(&CameraConfig::default()).unwrap();
        let mut p = Params::default();
        a.initial(&mut p);
        let flat = p.lens_shading.clone().unwrap();
        assert!(flat.r.iter().all(|&v| v == 1.0));
        let s = stats(w, h, 1.02);
        let meta = |f| FrameMetadata::new(f, Default::default(), 1.0, Default::default());
        let red = |p: &Params| {
            let t = p.lens_shading.as_ref().unwrap();
            t.r[2 * 8 + 6] / t.r[2 * 8 + 1]
        };
        // Frame 0's run is taken on frame 1, at once during start-up.
        a.process(&s, &meta(0), &mut p);
        assert_eq!(red(&p), 1.0);
        a.process(&s, &meta(1), &mut p);
        let r1 = red(&p);
        assert!(r1 < 0.995, "{r1}");
        for f in 2..12 {
            a.process(&s, &meta(f), &mut p);
        }
        let settled = red(&p);
        assert!(settled < r1);
        // A changed scene: a run on the next period, then halfway per frame.
        let s2 = stats(w, h, 1.0);
        let mut seen = Vec::new();
        for f in 12..20 {
            a.process(&s2, &meta(f), &mut p);
            seen.push(red(&p));
        }
        // (The first frame may still take a run on the old scene.)
        assert!(seen[1..].windows(2).all(|x| x[1] >= x[0]), "{seen:?}");
        assert!(seen.last().unwrap() > &settled);
        assert!(a.last_iterations().0 > 0);
    }

    #[test]
    fn statistics_on_another_grid_keep_the_calibration() {
        let mut a = Alsc::new(AlscTuning {
            grid: (8, 6),
            ..Default::default()
        })
        .unwrap();
        a.prepare(&CameraConfig::default()).unwrap();
        let mut p = Params::default();
        for f in 0..20 {
            let meta = FrameMetadata::new(f, Default::default(), 1.0, Default::default());
            a.process(&stats(16, 12, 1.05), &meta, &mut p);
        }
        assert_eq!(p.lens_shading.unwrap(), a.tables(p.colour_temperature));
    }
}
