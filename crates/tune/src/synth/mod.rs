//! A synthetic sensor for validating the calibration: known colour response, lens shading,
//! black level and noise, rendering dark frames, flat fields and ColorChecker shots as raw
//! Bayer frames. The calibration run on its frames must give back what it was built with.
//!
//! Two colour responses:
//!
//! * [`Response::Matrix`]: camera RGB = `diag(r(T), 1, b(T)) × M(T)⁻¹ × reference`, with `M`
//!   rows summing to 1. The true CT curve is `(r(T), b(T))` and the true colour matrix `M(T)`,
//!   exactly.
//! * [`Response::Spectral`]: Gaussian channel sensitivities, black-body light, reflectance
//!   spectra ([`spectral`]); the true CT curve is a grey's R/G and B/G, the best matrix is what
//!   the fit gives on noise-free patch values.
//!
//! Lens shading: each channel falls off as `1 / (1 + a ρ² + b ρ⁴)` (ρ the distance from the
//! optical centre over the half diagonal), red and blue with their own `a` that moves with
//! the colour temperature. Noise: shot noise of `electrons_per_code` electrons per code at
//! gain 1, read noise in codes (× gain), quantisation; optional stuck pixels.

pub mod spectral;

use crate::colour::macbeth_linear;
use crate::linalg::{Mat3, apply, inverse, mul_vec};
use crate::raw::{Cfa, GB, RawFrame};

/// Deterministic random numbers (SplitMix64, Box-Muller).
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Seeded.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal.
    pub fn normal(&mut self) -> f64 {
        let u = self.uniform().max(1e-300);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

/// A colour response with exact truth: CT curve and colour matrices.
#[derive(Clone, Debug, PartialEq)]
pub struct MatrixResponse {
    /// `(ct, r, b)`: R/G and B/G of a grey, linear in mired between points.
    pub ct_curve: Vec<[f64; 3]>,
    /// `(ct, matrix)`, rows summing to 1, linear in temperature between points.
    pub ccms: Vec<(f64, Mat3)>,
}

impl Default for MatrixResponse {
    fn default() -> Self {
        Self {
            ct_curve: vec![
                [2800.0, 0.95, 0.42],
                [4000.0, 0.72, 0.58],
                [6500.0, 0.52, 0.78],
            ],
            ccms: vec![
                (
                    2800.0,
                    [1.90, -0.70, -0.20, -0.45, 1.75, -0.30, 0.05, -0.95, 1.90],
                ),
                (
                    6500.0,
                    [1.65, -0.50, -0.15, -0.30, 1.55, -0.25, 0.00, -0.55, 1.55],
                ),
            ],
        }
    }
}

impl MatrixResponse {
    /// `(r, b)` at a temperature.
    pub fn curve(&self, ct: f64) -> (f64, f64) {
        let c = &self.ct_curve;
        let m = 1e6 / ct;
        let i = c
            .iter()
            .position(|p| p[0] >= ct)
            .unwrap_or(c.len() - 1)
            .max(1);
        let (a, b) = (c[i - 1], c[i]);
        let t = ((m - 1e6 / a[0]) / (1e6 / b[0] - 1e6 / a[0])).clamp(0.0, 1.0);
        (a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t)
    }

    /// The true colour matrix at a temperature.
    pub fn ccm(&self, ct: f64) -> Mat3 {
        let c = &self.ccms;
        if ct <= c[0].0 {
            return c[0].1;
        }
        if ct >= c[c.len() - 1].0 {
            return c[c.len() - 1].1;
        }
        let i = c.iter().position(|p| p.0 >= ct).unwrap_or(1);
        let t = (ct - c[i - 1].0) / (c[i].0 - c[i - 1].0);
        std::array::from_fn(|k| c[i - 1].1[k] + (c[i].1[k] - c[i - 1].1[k]) * t)
    }
}

/// How the sensor sees colour.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    /// Exact CT curve and matrices.
    Matrix(MatrixResponse),
    /// Physical: sensitivities, black-body light, reflectance spectra.
    Spectral(spectral::Sensitivities),
}

/// Lens shading: per channel `1 / (1 + a ρ² + b ρ⁴)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Shading {
    /// Green `a`.
    pub a: f64,
    /// `b` (all channels).
    pub b: f64,
    /// Red `a` − green `a` at 3000 K and at 6000 K (linear in between).
    pub red: (f64, f64),
    /// Blue `a` − green `a` at 3000 K and at 6000 K.
    pub blue: (f64, f64),
    /// Optical centre, fraction of the width and height.
    pub centre: (f64, f64),
}

impl Default for Shading {
    fn default() -> Self {
        Self {
            a: 0.8,
            b: 0.35,
            red: (0.25, 0.1),
            blue: (-0.1, 0.05),
            centre: (0.52, 0.48),
        }
    }
}

impl Shading {
    /// Gain of channel (0 R, 1 G, 2 B) at full-resolution `(x, y)` of a `w × h` frame.
    pub fn gain(&self, c: usize, x: f64, y: f64, w: f64, h: f64, ct: f64) -> f64 {
        let (dx, dy) = (x - self.centre.0 * w, y - self.centre.1 * h);
        let r2 = (dx * dx + dy * dy) / ((w * w + h * h) / 4.0);
        let t = ((ct - 3000.0) / 3000.0).clamp(0.0, 1.0);
        let a = self.a
            + match c {
                0 => self.red.0 + (self.red.1 - self.red.0) * t,
                2 => self.blue.0 + (self.blue.1 - self.blue.0) * t,
                _ => 0.0,
            };
        1.0 / (1.0 + a * r2 + self.b * r2 * r2)
    }
}

/// The synthetic sensor.
#[derive(Clone, Debug, PartialEq)]
pub struct SensorModel {
    /// Samples across.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// Mosaic.
    pub cfa: Cfa,
    /// Bits per sample.
    pub bits: u8,
    /// Black level per channel (R, Gr, Gb, B) at gain 1, normalised.
    pub black: [f64; 4],
    /// Black level change per unit of gain above 1, normalised.
    pub black_per_gain: f64,
    /// Electrons per code at gain 1.
    pub electrons_per_code: f64,
    /// Read noise at gain 1, codes (grows with the gain).
    pub read_noise: f64,
    /// Gr/Gb imbalance: Gb's response relative to Gr's.
    pub green_imbalance: f64,
    /// Colour response.
    pub response: Response,
    /// Lens shading.
    pub shading: Shading,
    /// Stuck pixels `(x, y)`, always at full scale.
    pub hot_pixels: Vec<(usize, usize)>,
}

impl Default for SensorModel {
    fn default() -> Self {
        let b = 64.0 / 1024.0;
        Self {
            width: 640,
            height: 400,
            cfa: Cfa::Bggr,
            bits: 10,
            black: [b + 0.0004, b, b + 0.0002, b + 0.0006],
            black_per_gain: 0.0003,
            electrons_per_code: 2.5,
            read_noise: 0.7,
            green_imbalance: 1.0,
            response: Response::Matrix(MatrixResponse::default()),
            shading: Shading::default(),
            hot_pixels: vec![(101, 77), (333, 250)],
        }
    }
}

/// What is in front of the camera.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Lens covered.
    Dark,
    /// A uniform diffuser of this reflectance.
    Flat(f64),
    /// The ColorChecker: chart coordinates (patch column, row) → full-resolution pixels, on a
    /// background of this grey reflectance.
    Chart {
        /// Placement.
        h: Mat3,
        /// Background reflectance.
        background: f64,
    },
}

/// One shot's conditions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shot {
    /// Colour temperature of the light.
    pub ct: f64,
    /// Electrons per microsecond a perfect white collects on the optical axis at gain 1.
    pub light: f64,
    /// Exposure, microseconds.
    pub exposure_us: f64,
    /// Analogue gain.
    pub gain: f64,
}

/// A chart placement: `pitch` pixels between patch centres, turned by `angle` degrees, a little
/// perspective, centred at `(cx, cy)` (full-resolution pixels).
pub fn chart_placement(cx: f64, cy: f64, pitch: f64, angle: f64, tilt: f64) -> Mat3 {
    let (s, c) = angle.to_radians().sin_cos();
    // Chart coordinates centred on the middle of the 6×4 patches.
    let centre: Mat3 = [1.0, 0.0, -2.5, 0.0, 1.0, -1.5, 0.0, 0.0, 1.0];
    let rot: Mat3 = [
        pitch * c,
        -pitch * s,
        0.0,
        pitch * s,
        pitch * c,
        0.0,
        tilt,
        0.0,
        1.0,
    ];
    let shift: Mat3 = [1.0, 0.0, cx, 0.0, 1.0, cy, 0.0, 0.0, 1.0];
    crate::linalg::mul(&shift, &crate::linalg::mul(&rot, &centre))
}

impl SensorModel {
    /// Camera RGB (relative to a perfect white's green) of the 24 patches under `ct`.
    pub fn patch_rgb(&self, ct: f64) -> Vec<[f64; 3]> {
        match &self.response {
            Response::Matrix(m) => {
                let (r, b) = m.curve(ct);
                let inv = inverse(&m.ccm(ct)).unwrap_or(crate::linalg::IDENTITY);
                macbeth_linear()
                    .iter()
                    .map(|p| {
                        let v = mul_vec(&inv, *p);
                        [v[0] * r, v[1], v[2] * b]
                    })
                    .collect()
            }
            Response::Spectral(s) => spectral::macbeth_spectra()
                .iter()
                .map(|spec| spectral::camera_rgb(s, spec, ct))
                .collect(),
        }
    }

    /// Camera RGB of a perfect white under `ct` (G = 1).
    pub fn white_rgb(&self, ct: f64) -> [f64; 3] {
        match &self.response {
            Response::Matrix(m) => {
                let (r, b) = m.curve(ct);
                [r, 1.0, b]
            }
            Response::Spectral(s) => {
                let flat = vec![1.0; spectral::wavelengths().count()];
                spectral::camera_rgb(s, &flat, ct)
            }
        }
    }

    /// The black level per channel at a gain.
    pub fn black_at(&self, gain: f64) -> [f64; 4] {
        self.black.map(|b| b + self.black_per_gain * (gain - 1.0))
    }

    /// Render one frame.
    pub fn render(&self, target: &Target, shot: &Shot, rng: &mut Rng) -> RawFrame {
        let (w, h) = (self.width, self.height);
        let full = f64::from(1u32 << self.bits);
        let black = self.black_at(shot.gain);
        let patches = self.patch_rgb(shot.ct);
        let white = self.white_rgb(shot.ct);
        let inv = match target {
            Target::Chart { h, .. } => inverse(h),
            _ => None,
        };
        let frame_rgb = |x: f64, y: f64| -> [f64; 3] {
            match target {
                Target::Dark => [0.0; 3],
                Target::Flat(r) => white.map(|v| v * r),
                Target::Chart { background, .. } => {
                    let [u, v] = apply(inv.as_ref().expect("chart"), [x, y]);
                    let (cu, cv) = (u.round(), v.round());
                    let inside_patch = (0.0..=5.0).contains(&cu)
                        && (0.0..=3.0).contains(&cv)
                        && (u - cu).abs() < 0.4
                        && (v - cv).abs() < 0.4;
                    if inside_patch {
                        patches[(cv as usize) * 6 + cu as usize]
                    } else if (-0.65..5.65).contains(&u) && (-0.65..3.65).contains(&v) {
                        white.map(|c| c * 0.03)
                    } else {
                        // A soft gradient so the background is not one flat region.
                        let g = background * (0.85 + 0.3 * (x / w as f64));
                        white.map(|c| c * g)
                    }
                }
            }
        };
        let mut data = vec![0u16; w * h];
        let gain = shot.gain;
        for y in 0..h {
            for x in 0..w {
                let ch = self.cfa.channel(x, y);
                let c3 = match ch {
                    0 => 0,
                    3 => 2,
                    _ => 1,
                };
                let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                let mut signal = frame_rgb(fx, fy)[c3]
                    * self.shading.gain(c3, fx, fy, w as f64, h as f64, shot.ct)
                    * shot.light
                    * shot.exposure_us;
                if ch == GB {
                    signal *= self.green_imbalance;
                }
                let electrons = (signal + signal.max(0.0).sqrt() * rng.normal()).max(0.0);
                let codes = electrons * gain / self.electrons_per_code
                    + self.read_noise * gain * rng.normal()
                    + black[ch] * full;
                data[y * w + x] = codes.round().clamp(0.0, full - 1.0) as u16;
            }
        }
        for &(x, y) in &self.hot_pixels {
            if x < w && y < h {
                data[y * w + x] = (full - 1.0) as u16;
            }
        }
        RawFrame {
            width: w,
            height: h,
            cfa: self.cfa,
            bits: self.bits,
            data,
            exposure_us: shot.exposure_us,
            analogue_gain: gain,
            digital_gain: 1.0,
            black_level: None,
        }
    }

    /// `n` frames of the same shot.
    pub fn burst(&self, target: &Target, shot: &Shot, n: usize, seed: u64) -> Vec<RawFrame> {
        let mut rng = Rng::new(seed);
        (0..n)
            .map(|_| self.render(target, shot, &mut rng))
            .collect()
    }

    /// The true noise at gain 1 in the Raspberry Pi model's units (16-bit codes): the standard
    /// deviation at a signal of `level` (16-bit codes above black).
    pub fn noise_16bit(&self, level: f64) -> f64 {
        let scale = f64::from(1u32 << (16 - u32::from(self.bits)));
        let codes = level / scale;
        // Quantisation adds 1/12 code².
        scale * (codes / self.electrons_per_code + self.read_noise.powi(2) + 1.0 / 12.0).sqrt()
    }
}
