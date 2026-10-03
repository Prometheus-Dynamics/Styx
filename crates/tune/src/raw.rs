//! Raw frames and the per-channel planes the calibration works on.
//!
//! A [`RawFrame`] is one Bayer mosaic as the sensor delivered it (samples at their bit depth)
//! with the exposure and gains that produced it. [`Planes`] splits a frame (or the mean of a
//! burst of frames, [`Burst`]) into its four channels R, Gr, Gb, B at half resolution (one
//! value per 2×2 quad), normalised to full scale 1.0 (`sample / 2^bits`, so 64 in 10 bits is
//! 0.0625, as styx-algo's levels are).

use serde::{Deserialize, Serialize};

/// Channel indices in [`Planes::ch`] and every `[_; 4]` per-channel array.
pub const R: usize = 0;
/// Green on the red rows.
pub const GR: usize = 1;
/// Green on the blue rows.
pub const GB: usize = 2;
/// Blue.
pub const B: usize = 3;

/// The colour filter array's top-left 2×2 quad, row by row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cfa {
    /// R G / G B.
    Rggb,
    /// G R / B G.
    Grbg,
    /// G B / R G.
    Gbrg,
    /// B G / G R.
    Bggr,
}

impl Cfa {
    /// The channel ([`R`], [`GR`], [`GB`], [`B`]) of the sample at `(x, y)`.
    pub fn channel(self, x: usize, y: usize) -> usize {
        let (odd_x, odd_y) = (x & 1 == 1, y & 1 == 1);
        // Position of red in the quad.
        let (rx, ry) = match self {
            Cfa::Rggb => (false, false),
            Cfa::Grbg => (true, false),
            Cfa::Gbrg => (false, true),
            Cfa::Bggr => (true, true),
        };
        match (odd_x == rx, odd_y == ry) {
            (true, true) => R,
            (false, true) => GR,
            (true, false) => GB,
            (false, false) => B,
        }
    }

    /// From a name (`rggb`, `BGGR`, ...).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "rggb" => Some(Cfa::Rggb),
            "grbg" => Some(Cfa::Grbg),
            "gbrg" => Some(Cfa::Gbrg),
            "bggr" => Some(Cfa::Bggr),
            _ => None,
        }
    }

    /// From the software ISP's pattern.
    pub fn from_softisp(p: styx_softisp::CfaPattern) -> Self {
        use styx_softisp::CfaPattern as P;
        match p {
            P::Rggb => Cfa::Rggb,
            P::Grbg => Cfa::Grbg,
            P::Gbrg => Cfa::Gbrg,
            P::Bggr => Cfa::Bggr,
        }
    }
}

/// One raw Bayer frame and what produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct RawFrame {
    /// Samples across.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// Mosaic.
    pub cfa: Cfa,
    /// Bits per sample.
    pub bits: u8,
    /// Row-major samples, `width × height`.
    pub data: Vec<u16>,
    /// Exposure time, microseconds (0 when unknown).
    pub exposure_us: f64,
    /// Analogue gain (1 when unknown).
    pub analogue_gain: f64,
    /// Sensor digital gain (1 without one).
    pub digital_gain: f64,
    /// The black level the file states, normalised, if any.
    pub black_level: Option<f64>,
}

impl RawFrame {
    /// Total sensor gain.
    pub fn gain(&self) -> f64 {
        self.analogue_gain * self.digital_gain
    }

    /// `2^bits`: the normalisation of every level.
    pub fn full_scale(&self) -> f64 {
        f64::from(1u32 << self.bits)
    }

    /// Exposure × gain, the factor the signal scales with.
    pub fn exposure_gain(&self) -> f64 {
        self.exposure_us * self.gain()
    }
}

/// Four half-resolution planes, normalised to full scale 1.0.
#[derive(Clone, Debug, PartialEq)]
pub struct Planes {
    /// Quads across.
    pub width: usize,
    /// Quads down.
    pub height: usize,
    /// R, Gr, Gb, B, each `width × height`, row-major.
    pub ch: [Vec<f32>; 4],
}

impl Planes {
    /// The planes of one frame.
    pub fn from_frame(f: &RawFrame) -> Self {
        let mut acc = Accumulator::new(f);
        acc.add(f, 1.0);
        acc.mean()
    }

    /// Value of channel `c` at quad `(x, y)`.
    pub fn at(&self, c: usize, x: usize, y: usize) -> f32 {
        self.ch[c][y * self.width + x]
    }

    /// Subtract per-channel black levels.
    pub fn subtract(&mut self, black: [f64; 4]) {
        for (plane, b) in self.ch.iter_mut().zip(black) {
            plane.iter_mut().for_each(|v| *v -= b as f32);
        }
    }

    /// Per-channel trimmed means over the quads `x0..x1 × y0..y1` (clamped to the planes):
    /// values further than 4 robust deviations from the median are left out, so a defective
    /// pixel or an edge does not move it. `None` for an empty region.
    pub fn region(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> Option<[f64; 4]> {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let mut out = [0.0; 4];
        let mut buf = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for (c, o) in out.iter_mut().enumerate() {
            buf.clear();
            for y in y0..y1 {
                buf.extend_from_slice(&self.ch[c][y * self.width + x0..y * self.width + x1]);
            }
            *o = trimmed_mean(&mut buf);
        }
        Some(out)
    }

    /// Mean of each channel over everything.
    pub fn mean(&self) -> [f64; 4] {
        self.ch
            .each_ref()
            .map(|p| p.iter().map(|&v| f64::from(v)).sum::<f64>() / p.len().max(1) as f64)
    }
}

/// The trimmed mean of `v` (reordered): within 4 robust deviations (1.4826 × MAD) of the median.
pub fn trimmed_mean(v: &mut [f32]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let med = median(v);
    let mut dev: Vec<f32> = v.iter().map(|x| (x - med).abs()).collect();
    let mad = median(&mut dev) * 1.4826;
    let lim = (4.0 * mad).max(1e-6);
    let (mut s, mut n) = (0.0, 0usize);
    for &x in v.iter() {
        if (x - med).abs() <= lim {
            s += f64::from(x);
            n += 1;
        }
    }
    if n == 0 { f64::from(med) } else { s / n as f64 }
}

/// The median of `v` (reordered).
pub fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, f32::total_cmp);
    *m
}

/// Sums frames into planes (with weights), and their squares for the temporal variance.
struct Accumulator {
    width: usize,
    height: usize,
    sum: [Vec<f64>; 4],
    sq: [Vec<f64>; 4],
    n: f64,
}

impl Accumulator {
    fn new(f: &RawFrame) -> Self {
        let (w, h) = (f.width / 2, f.height / 2);
        Self {
            width: w,
            height: h,
            sum: std::array::from_fn(|_| vec![0.0; w * h]),
            sq: std::array::from_fn(|_| vec![0.0; w * h]),
            n: 0.0,
        }
    }

    /// Add a frame, its samples multiplied by `scale`.
    fn add(&mut self, f: &RawFrame, scale: f64) {
        let k = scale / f.full_scale();
        for y in 0..self.height * 2 {
            let row = &f.data[y * f.width..y * f.width + self.width * 2];
            for (x, &v) in row.iter().enumerate() {
                let c = f.cfa.channel(x, y);
                let i = (y / 2) * self.width + x / 2;
                let v = f64::from(v) * k;
                self.sum[c][i] += v;
                self.sq[c][i] += v * v;
            }
        }
        self.n += 1.0;
    }

    fn mean(&self) -> Planes {
        let n = self.n.max(1.0);
        Planes {
            width: self.width,
            height: self.height,
            ch: self
                .sum
                .each_ref()
                .map(|s| s.iter().map(|v| (v / n) as f32).collect()),
        }
    }

    /// Unbiased per-pixel variance.
    fn variance(&self) -> Option<Planes> {
        if self.n < 2.0 {
            return None;
        }
        let n = self.n;
        let ch = std::array::from_fn(|c| {
            self.sum[c]
                .iter()
                .zip(&self.sq[c])
                .map(|(s, q)| ((q - s * s / n) / (n - 1.0)).max(0.0) as f32)
                .collect()
        });
        Some(Planes {
            width: self.width,
            height: self.height,
            ch,
        })
    }
}

/// A burst of frames taken with the same settings: their mean and, with two frames or more,
/// each pixel's variance across them (the temporal noise).
#[derive(Clone, Debug)]
pub struct Burst {
    /// Mean of the frames (black level not removed).
    pub mean: Planes,
    /// Per-pixel variance across the frames, after scaling each frame to the burst's mean
    /// brightness (so lamp flicker does not count as noise).
    pub variance: Option<Planes>,
    /// Frames.
    pub frames: usize,
    /// Exposure of the frames, microseconds.
    pub exposure_us: f64,
    /// Analogue gain.
    pub analogue_gain: f64,
    /// Digital gain.
    pub digital_gain: f64,
    /// Bits per sample.
    pub bits: u8,
    /// Mosaic.
    pub cfa: Cfa,
    /// Full-resolution size.
    pub size: (usize, usize),
}

impl Burst {
    /// The burst of `frames` (all the same size). `black` (normalised) is taken off before the
    /// brightness of each frame is compared, for the flicker scaling.
    pub fn new(frames: &[RawFrame], black: f64) -> Option<Self> {
        let first = frames.first()?;
        if frames.iter().any(|f| {
            (f.width, f.height, f.cfa, f.bits) != (first.width, first.height, first.cfa, first.bits)
        }) {
            return None;
        }
        let means: Vec<f64> = frames.iter().map(|f| frame_mean(f) - black).collect();
        let avg = means.iter().sum::<f64>() / means.len() as f64;
        let mut acc = Accumulator::new(first);
        let mut flat = Accumulator::new(first);
        for (f, m) in frames.iter().zip(&means) {
            acc.add(f, 1.0);
            // Scale the signal above black to the burst's mean brightness.
            let s = if *m > 1e-4 && avg > 1e-4 {
                avg / m
            } else {
                1.0
            };
            flat.add_scaled_above(f, s, black);
        }
        Some(Self {
            mean: acc.mean(),
            variance: flat.variance(),
            frames: frames.len(),
            exposure_us: first.exposure_us,
            analogue_gain: first.analogue_gain,
            digital_gain: first.digital_gain,
            bits: first.bits,
            cfa: first.cfa,
            size: (first.width, first.height),
        })
    }

    /// Total sensor gain.
    pub fn gain(&self) -> f64 {
        self.analogue_gain * self.digital_gain
    }
}

impl Accumulator {
    /// Add `black + (v - black) × s`.
    fn add_scaled_above(&mut self, f: &RawFrame, s: f64, black: f64) {
        let k = 1.0 / f.full_scale();
        for y in 0..self.height * 2 {
            let row = &f.data[y * f.width..y * f.width + self.width * 2];
            for (x, &v) in row.iter().enumerate() {
                let c = f.cfa.channel(x, y);
                let i = (y / 2) * self.width + x / 2;
                let v = black + (f64::from(v) * k - black) * s;
                self.sum[c][i] += v;
                self.sq[c][i] += v * v;
            }
        }
        self.n += 1.0;
    }
}

/// Mean of every sample, normalised.
pub fn frame_mean(f: &RawFrame) -> f64 {
    f.data.iter().map(|&v| f64::from(v)).sum::<f64>() / f.data.len().max(1) as f64 / f.full_scale()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(cfa: Cfa, f: impl Fn(usize, usize) -> u16) -> RawFrame {
        let (w, h) = (8, 6);
        RawFrame {
            width: w,
            height: h,
            cfa,
            bits: 10,
            data: (0..w * h).map(|i| f(i % w, i / w)).collect(),
            exposure_us: 1000.0,
            analogue_gain: 1.0,
            digital_gain: 1.0,
            black_level: None,
        }
    }

    #[test]
    fn channels_follow_the_pattern() {
        assert_eq!(Cfa::Rggb.channel(0, 0), R);
        assert_eq!(Cfa::Rggb.channel(1, 0), GR);
        assert_eq!(Cfa::Rggb.channel(0, 1), GB);
        assert_eq!(Cfa::Bggr.channel(0, 0), B);
        assert_eq!(Cfa::Bggr.channel(1, 1), R);
        assert_eq!(Cfa::Bggr.channel(1, 0), GB);
        assert_eq!(Cfa::Grbg.channel(1, 0), R);
        assert_eq!(Cfa::Gbrg.channel(0, 1), R);
        assert_eq!(Cfa::Gbrg.channel(1, 1), GR);
    }

    #[test]
    fn planes_split_and_normalise() {
        let codes = [100u16, 200, 300, 400];
        let f = frame(Cfa::Grbg, |x, y| codes[Cfa::Grbg.channel(x, y)]);
        let p = Planes::from_frame(&f);
        assert_eq!((p.width, p.height), (4, 3));
        for (c, code) in codes.iter().enumerate() {
            assert!((p.at(c, 1, 2) - *code as f32 / 1024.0).abs() < 1e-6);
        }
        let r = p.region(0, 0, 4, 3).unwrap();
        assert!((r[B] - 400.0 / 1024.0).abs() < 1e-6);
    }

    #[test]
    fn burst_variance_ignores_flicker() {
        let frames: Vec<RawFrame> = [1.0, 1.2, 0.9]
            .iter()
            .map(|k| frame(Cfa::Rggb, |_, _| (64.0 + 400.0 * k) as u16))
            .collect();
        let b = Burst::new(&frames, 64.0 / 1024.0).unwrap();
        let v = b.variance.unwrap();
        assert!(v.ch[R].iter().all(|v| *v < 1e-9), "{:?}", v.ch[R]);
        assert!((b.mean.at(R, 0, 0) - (64.0 + 400.0 * 31.0 / 30.0) / 1024.0).abs() < 1e-3);
    }

    #[test]
    fn trimmed_mean_drops_outliers() {
        let mut v = vec![1.0f32; 99];
        v.push(100.0);
        assert!((trimmed_mean(&mut v) - 1.0).abs() < 1e-9);
    }
}
