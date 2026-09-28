//! ALSC: lens-shading tables interpolated by colour temperature.
//!
//! Ported from Raspberry Pi's `alsc.cpp` (BSD-2-Clause, Copyright (C) 2019 Raspberry Pi Ltd):
//! calibration interpolation (`getCalTable`), resampling to the mode's crop and flips
//! (`resampleCalTable`), normalising (`compensateLambdasForCal`), the luminance table and its
//! strength (`addLuminanceToTables`, `generateLut`). The adaptive part (the Gauss-Seidel
//! refinement from statistics) is not ported: tables follow the calibration only, as the
//! original does with `n_iter = 0`.

use serde::{Deserialize, Serialize};

use crate::config::{CameraConfig, Crop};
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
use crate::params::{LensShading, Params};
use crate::pipeline::Algorithm;
use crate::stats::Statistics;

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
            if cals.iter().any(|c| c.table.iter().any(|v| !(*v > 0.0))) {
                return err(format!("calibrations_{name}: gains must be positive"));
            }
        }
        if !self.luminance_lut.is_empty() && self.luminance_lut.len() != self.cells() {
            return err("luminance_lut size does not match grid".into());
        }
        if self.corner_strength.is_some_and(|c| c <= 1.0) || self.asymmetry < 0.0 {
            return err("corner_strength must be > 1 and asymmetry >= 0".into());
        }
        Ok(())
    }

    /// The luminance table: tuned, generated, or unity.
    fn luminance(&self) -> Vec<f64> {
        if !self.luminance_lut.is_empty() {
            return self.luminance_lut.clone();
        }
        let (w, h) = (self.grid.0 as i64, self.grid.1 as i64);
        let Some(cs) = self.corner_strength else {
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

/// The ALSC algorithm.
#[derive(Debug, Clone)]
pub struct Alsc {
    tuning: AlscTuning,
    config: CameraConfig,
    luminance: Vec<f64>,
    cached: Option<(f64, LensShading)>,
}

impl Alsc {
    /// An ALSC algorithm.
    pub fn new(tuning: AlscTuning) -> Result<Self> {
        tuning.validate()?;
        Ok(Self {
            tuning,
            config: CameraConfig::default(),
            luminance: Vec::new(),
            cached: None,
        })
    }

    /// Tables for a colour temperature.
    pub fn tables(&self, ct: f64) -> LensShading {
        let t = &self.tuning;
        let n = t.cells();
        let c = &self.config;
        let prep = |cals: &[AlscCalibration]| {
            let mut v = resample(&cal_table(cals, ct, n), t.grid, c.crop, c.hflip, c.vflip);
            let min = v.iter().copied().fold(f64::INFINITY, f64::min);
            v.iter_mut().for_each(|x| *x /= min);
            v
        };
        let lum = |x: f64| (x - 1.0) * t.luminance_strength + 1.0;
        let r = prep(&t.calibrations_cr);
        let b = prep(&t.calibrations_cb);
        LensShading {
            width: t.grid.0,
            height: t.grid.1,
            r: r.iter()
                .zip(&self.luminance)
                .map(|(a, l)| a * lum(*l))
                .collect(),
            g: self.luminance.iter().map(|l| lum(*l)).collect(),
            b: b.iter()
                .zip(&self.luminance)
                .map(|(a, l)| a * lum(*l))
                .collect(),
        }
    }
}

impl Algorithm for Alsc {
    fn name(&self) -> &'static str {
        "alsc"
    }

    fn prepare(&mut self, config: &CameraConfig) -> Result<()> {
        self.config = config.clone();
        let c = &self.config;
        self.luminance = resample(
            &self.tuning.luminance(),
            self.tuning.grid,
            c.crop,
            c.hflip,
            c.vflip,
        );
        self.cached = None;
        Ok(())
    }

    fn initial(&self, params: &mut Params) {
        params.lens_shading = Some(self.tables(self.tuning.default_ct));
    }

    fn process(&mut self, _: &Statistics, _: &FrameMetadata, params: &mut Params) {
        let ct = params.colour_temperature;
        let tables = match &self.cached {
            Some((c, t)) if *c == ct => t.clone(),
            _ => {
                let t = self.tables(ct);
                self.cached = Some((ct, t.clone()));
                t
            }
        };
        params.lens_shading = Some(tables);
    }
}

#[cfg(test)]
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
}
