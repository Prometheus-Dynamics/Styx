//! Synthetic dark captures with known answers, for the tests and for checking the analysis
//! before a board capture exists.
//!
//! Each sample is the channel's black level, plus the static fixed pattern of its pixel, plus
//! the row's and column's offset in that frame, plus dark current (per second of exposure),
//! plus Gaussian read noise; defects are injected on top. The generator is deterministic (a
//! xorshift stream), so the same spec always gives the same frames.

use std::path::PathBuf;

use crate::raw::{Cfa, FrameRow, RawCapture};

/// What to generate.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// Width and height.
    pub size: (usize, usize),
    /// Frames.
    pub frames: usize,
    /// The CFA.
    pub cfa: Cfa,
    /// Bits per sample.
    pub bits: u32,
    /// Black level per channel (R, Gr, Gb, B), raw DN.
    pub black: [f64; 4],
    /// Temporal read noise, raw DN per sample (standard deviation).
    pub read_noise: f64,
    /// Static fixed pattern per pixel, raw DN (standard deviation over pixels).
    pub fpn: f64,
    /// Row offset per frame, raw DN (standard deviation over rows, redrawn each frame).
    pub row_noise: f64,
    /// Dark current, raw DN per second of exposure.
    pub dark_current: f64,
    /// Exposure, microseconds.
    pub exposure_us: f64,
    /// Analogue gain.
    pub analog_gain: f64,
    /// Pixels whose mean is raised, `(x, y, DN)`.
    pub hot: Vec<(usize, usize, f64)>,
    /// Pixels stuck at a value, `(x, y, value)`.
    pub stuck: Vec<(usize, usize, u16)>,
    /// Random seed.
    pub seed: u64,
}

impl Spec {
    /// A 128 × 96 BGGR capture of 40 frames at 10 bits, with a clean pedestal of 64 and
    /// read noise of 2 DN: a base to change.
    pub fn base() -> Self {
        Self {
            size: (128, 96),
            frames: 40,
            cfa: Cfa::parse("BGGR").expect("cfa"),
            bits: 10,
            black: [64.0, 64.0, 64.0, 64.0],
            read_noise: 2.0,
            fpn: 0.0,
            row_noise: 0.0,
            dark_current: 0.0,
            exposure_us: 8000.0,
            analog_gain: 1.0,
            hot: Vec::new(),
            stuck: Vec::new(),
            seed: 1,
        }
    }
}

/// A small deterministic generator (xorshift64*).
pub struct Rng(u64);

impl Rng {
    /// Seeded (zero is replaced).
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in (0, 1].
    pub fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }

    /// A standard normal draw (Box-Muller).
    pub fn gauss(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// The capture `spec` describes, in memory.
pub fn capture(spec: &Spec) -> RawCapture {
    let (w, h) = spec.size;
    let max = (1u32 << spec.bits) - 1;
    let mut rng = Rng::new(spec.seed);
    // The static pattern, drawn once.
    let fixed: Vec<f64> = (0..w * h).map(|_| spec.fpn * rng.gauss()).collect();
    let signal_s = spec.dark_current * spec.exposure_us * 1e-6;
    let mut samples = Vec::with_capacity(w * h * spec.frames);
    for _ in 0..spec.frames {
        let rows: Vec<f64> = (0..h).map(|_| spec.row_noise * rng.gauss()).collect();
        for y in 0..h {
            for x in 0..w {
                let c = spec.cfa.channel(x, y).index();
                let v = spec.black[c]
                    + fixed[y * w + x]
                    + rows[y]
                    + signal_s
                    + spec.read_noise * rng.gauss();
                let mut v = v.round().clamp(0.0, f64::from(max)) as u16;
                if let Some(&(_, _, value)) =
                    spec.stuck.iter().find(|&&(sx, sy, _)| sx == x && sy == y)
                {
                    v = value;
                }
                if let Some(&(_, _, add)) = spec.hot.iter().find(|&&(hx, hy, _)| hx == x && hy == y)
                {
                    v = (f64::from(v) + add).round().clamp(0.0, f64::from(max)) as u16;
                }
                samples.push(v);
            }
        }
    }
    let rows = (0..spec.frames)
        .map(|f| FrameRow {
            exposure_us: spec.exposure_us,
            analog_gain: spec.analog_gain,
            temperature: Some(40.0 + 0.1 * f as f64),
        })
        .collect();
    RawCapture::from_samples(
        PathBuf::from(format!(
            "synthetic-e{}-g{}",
            spec.exposure_us, spec.analog_gain
        )),
        spec.size,
        spec.cfa,
        spec.bits,
        &samples,
        rows,
    )
}
