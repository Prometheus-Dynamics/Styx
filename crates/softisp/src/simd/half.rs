//! Row kernels of [`Arithmetic::Half`](crate::Arithmetic::Half): working values are fp16
//! (bits in `u16`), 0..=4095 like the integer path's 12-bit samples but with a float's
//! relative precision. Each kernel has a scalar oracle built on [`f16`] (exact software
//! fp16) and an AArch64 leaf for CPUs with FP16 arithmetic (ARMv8.2: Cortex-A55/A76 and
//! later), which must match it bit for bit; without FP16 hardware the oracle runs (slowly).
//!
//! Why fp16: the Cortex-A76 issues fp16 multiply-adds on both vector pipes, 16 lanes a cycle,
//! but 16-bit integer multiplies on one pipe at half rate (4 lanes a cycle), and its
//! int-to-fp16 conversions take 4 cycles per 8 lanes. The front end therefore makes fp16
//! values without a conversion (a 10-bit sample `v` ORed into the mantissa of 1024.0 is the
//! fp16 `1024 + v`), the colour matrix is nine multiply-adds per 8 pixels, and the tone curve
//! is looked up with `tbl` from the fp16 bits themselves (see [`HalfTone`]).

use super::f16;
#[cfg(all(feature = "neon", target_arch = "aarch64"))]
use super::neon;
use crate::format::CfaPattern;

/// fp16 constants.
pub const H_HALF: u16 = 0x3800;
pub const H_QUARTER: u16 = 0x3400;
pub const H_16: u16 = 0x4C00;
/// The largest working value: 4094, the largest fp16 below 4096 (fp16 steps by 2 there).
pub const H_MAX: u16 = 0x6BFF;

/// Whether the FP16 leaves run on this CPU.
#[inline]
pub fn hardware() -> bool {
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    {
        std::arch::is_aarch64_feature_detected!("fp16")
    }
    #[cfg(not(all(feature = "neon", target_arch = "aarch64")))]
    {
        false
    }
}

/// A tone curve from fp16 working values to 8 bits. The input is `v + 16` (`v` the linear
/// value, 0..=4095, so the input is at least 16); its fp16 bits' high byte (exponent and two
/// mantissa bits) picks one of 48 segments, four per octave from 16 up, and the low byte is
/// the position within it: `out = (base 256 + 128 + slope low) >> 8`. Segments are as fine
/// as the curve needs where gamma curves are steep: 1 code wide just above black, 1024 at the
/// top. Against the integer path's 257-node table: at most 1 code apart on the sRGB curve,
/// 2 on the HeliOS (Raspberry Pi contrast) curve, 0.4-0.5 rms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HalfTone {
    pub base: [u8; Self::SEGMENTS],
    /// `base[i + 1] - base[i]`.
    pub slope: [u8; Self::SEGMENTS],
}

impl HalfTone {
    pub const SEGMENTS: usize = 48;
    /// High byte of the fp16 16.0: segment 0.
    pub const FIRST: u8 = (H_16 >> 8) as u8;

    /// Segments of `curve` (0..1 to 0..1); `None` when it falls anywhere (the segments need
    /// a non-negative slope).
    pub fn from_curve(curve: impl Fn(f32) -> f32) -> Option<Self> {
        let at = |i: usize| {
            let v = f16::to_f64(((i + Self::FIRST as usize) << 8) as u16) - 16.0;
            let x = (v / 4095.0).clamp(0.0, 1.0) as f32;
            (curve(x).clamp(0.0, 1.0) * 255.0).round() as i32
        };
        let mut base = [0u8; Self::SEGMENTS];
        let mut slope = [0u8; Self::SEGMENTS];
        for i in 0..Self::SEGMENTS {
            let (a, b) = (at(i), at(i + 1));
            if b < a {
                return None;
            }
            base[i] = a as u8;
            slope[i] = (b - a) as u8;
        }
        Some(Self { base, slope })
    }

    /// The output for the fp16 bits `acc` (at least 16.0).
    #[inline]
    pub fn apply(&self, acc: u16) -> u8 {
        let i = ((acc >> 8) as u8).saturating_sub(Self::FIRST) as usize;
        let (b, s) = match (self.base.get(i), self.slope.get(i)) {
            (Some(&b), Some(&s)) => (u32::from(b), u32::from(s)),
            _ => (0, 0),
        };
        ((b * 256 + 128 + s * u32::from(acc & 0xFF)) >> 8) as u8
    }
}

/// Colour matrix coefficients for the bilinear demosaic of one kind of row, with the
/// demosaic's averaging folded in: at each pixel the kernel forms three sums `(g, xs, ys)`:
/// at a green pixel the green sample, the two horizontal and the two vertical neighbours; at
/// another the four cross neighbours, the sample and the four diagonal neighbours. Output
/// channel `k` is `16 + sum_j c[k][j][column parity] * sum_j`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourCoeffs {
    pub c: [[[u16; 2]; 3]; 3],
    /// Green on even columns.
    pub green_even: bool,
}

impl ColourCoeffs {
    /// For a row of `kind` and the matrix `m` (row `k`: output `k` from R, G, B).
    pub fn new(m: &[[f32; 3]; 3], kind: super::RowKind) -> Self {
        let (x_col, y_col) = if kind.x_is_red { (0, 2) } else { (2, 0) };
        let cols = [1, x_col, y_col];
        let c = std::array::from_fn(|k| {
            std::array::from_fn(|j| {
                std::array::from_fn(|parity| {
                    let green = (parity == 0) == kind.green_even;
                    let scale = if green {
                        [1.0, 0.5, 0.5][j]
                    } else {
                        [0.25, 1.0, 0.25][j]
                    };
                    f16::from_f32(m[k][cols[j]] * scale)
                })
            })
        });
        Self {
            c,
            green_even: kind.green_even,
        }
    }
}

/// Where a colour kernel writes: three 8-bit planes, or packed RGB24.
pub enum ColourOut<'a> {
    Planes([&'a mut [u8]; 3]),
    Packed(&'a mut [u8]),
}

impl ColourOut<'_> {
    #[inline]
    fn put(&mut self, x: usize, rgb: [u8; 3]) {
        match self {
            Self::Planes(p) => {
                for (plane, v) in p.iter_mut().zip(rgb) {
                    plane[x] = v;
                }
            }
            Self::Packed(d) => d[3 * x..3 * x + 3].copy_from_slice(&rgb),
        }
    }
}

/// Lens shading gains of one row: `a + d t` per column.
#[derive(Clone, Copy)]
pub struct LscRow<'a> {
    pub a: &'a [u16],
    pub d: &'a [u16],
    pub t: u16,
}

#[cfg(test)]
thread_local! {
    /// Tests: run the scalar oracle alone.
    pub(crate) static SCALAR_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline(always)]
#[allow(dead_code)]
fn scalar_only() -> bool {
    #[cfg(test)]
    {
        SCALAR_ONLY.with(|s| s.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Run the FP16 leaf when the CPU has it, and finish with the scalar oracle from where it
/// stopped.
macro_rules! half_leaf {
    ($name:ident($($arg:expr),* $(,)?)) => {{
        #[allow(unused_mut)]
        let mut done = 0usize;
        #[cfg(all(feature = "neon", target_arch = "aarch64"))]
        if hardware() && !scalar_only() {
            // SAFETY: FP16 is available (checked above) and the dispatcher sized the slices
            // as the leaf documents.
            done = unsafe { neon::half::$name($($arg),*) };
        }
        done
    }};
}

/// Front end of one row of 10-bit (or narrower) samples: `min(max(1024 + v - black, 0) gain
/// [lsc], 4094)` with `black[column parity]` the fp16 of `1024 + black level` and `gain` the
/// fp16 channel gain (white balance, digital gain, range); with lens shading, times
/// `a + d t`.
pub fn front_row(
    raw: &[u16],
    dst: &mut [u16],
    black: [u16; 2],
    gain: [u16; 2],
    lsc: Option<LscRow>,
    width: usize,
) {
    let (raw, dst) = (&raw[..width], &mut dst[..width]);
    let lsc = lsc.map(|l| LscRow {
        a: &l.a[..width],
        d: &l.d[..width],
        t: l.t,
    });
    let done = half_leaf!(front(raw, dst, black, gain, lsc, width));
    debug_assert_eq!(done % 2, 0);
    for x in done..width {
        let p = x & 1;
        let d = f16::max(f16::sub(f16::biased(raw[x]), black[p]), 0);
        let mut y = f16::mul(d, gain[p]);
        if let Some(l) = &lsc {
            y = f16::mul(y, f16::fma(l.a[x], l.d[x], l.t));
        }
        dst[x] = f16::min(y, H_MAX);
    }
}

/// Bilinear demosaic, colour matrix and tone curve of one row (rows above, at and below from
/// column -1, as the integer kernels).
pub fn colour_row(
    rows: [&[u16]; 3],
    mut out: ColourOut,
    width: usize,
    cc: &ColourCoeffs,
    tone: &HalfTone,
) {
    let rows = rows.map(|r| &r[..width + 2]);
    let done = half_leaf!(colour(rows, &mut out, width, cc, tone));
    debug_assert_eq!(done % 2, 0);
    let [up, cur, dn] = rows;
    for x in done..width {
        let p = x & 1;
        let hs = f16::add(cur[x], cur[x + 2]);
        let vs = f16::add(up[x + 1], dn[x + 1]);
        let diag = f16::add(f16::add(up[x], up[x + 2]), f16::add(dn[x], dn[x + 2]));
        let sums = if (p == 0) == cc.green_even {
            [cur[x + 1], hs, vs]
        } else {
            [f16::add(hs, vs), cur[x + 1], diag]
        };
        let rgb = std::array::from_fn(|k| {
            let c = &cc.c[k];
            let acc = f16::fma(
                f16::fma(f16::fma(H_16, sums[0], c[0][p]), sums[1], c[1][p]),
                sums[2],
                c[2][p],
            );
            tone.apply(f16::max(acc, H_16))
        });
        out.put(x, rgb);
    }
}

/// Positions of red, the two greens and blue in a quad `[top-left, top-right, bottom-left,
/// bottom-right]`.
pub const fn quad_positions(pattern: CfaPattern) -> [usize; 4] {
    match pattern {
        CfaPattern::Rggb => [0, 1, 2, 3],
        CfaPattern::Bggr => [3, 1, 2, 0],
        CfaPattern::Grbg => [1, 0, 3, 2],
        CfaPattern::Gbrg => [2, 0, 3, 1],
    }
}

/// R, mean G, B of quad `i`.
#[inline]
fn quad(top: &[u16], bottom: &[u16], i: usize, pos: [usize; 4]) -> [u16; 3] {
    let q = [top[2 * i], top[2 * i + 1], bottom[2 * i], bottom[2 * i + 1]];
    [
        q[pos[0]],
        f16::mul(f16::add(q[pos[1]], q[pos[2]]), H_HALF),
        q[pos[3]],
    ]
}

/// Half size: each quad's R, mean G and B through the colour matrix `m` (fp16, row-major)
/// and the tone curve.
pub fn quad_colour_row(
    top: &[u16],
    bottom: &[u16],
    mut out: ColourOut,
    width: usize,
    m: &[u16; 9],
    pattern: CfaPattern,
    tone: &HalfTone,
) {
    let (top, bottom) = (&top[..2 * width], &bottom[..2 * width]);
    let done = half_leaf!(quad_colour(top, bottom, &mut out, width, m, pattern, tone));
    let pos = quad_positions(pattern);
    for i in done..width {
        let [r, g, b] = quad(top, bottom, i, pos);
        let rgb = std::array::from_fn(|k| {
            let acc = f16::fma(
                f16::fma(f16::fma(H_16, r, m[3 * k]), g, m[3 * k + 1]),
                b,
                m[3 * k + 2],
            );
            tone.apply(f16::max(acc, H_16))
        });
        out.put(i, rgb);
    }
}

/// Full-size luma from the mosaic (the 3x3 binomial filter, `(R + 2G + B) / 4` at every
/// pixel) through the tone curve. Rows as [`colour_row`].
pub fn luma_row(rows: [&[u16]; 3], dst: &mut [u8], width: usize, tone: &HalfTone) {
    let rows = rows.map(|r| &r[..width + 2]);
    let dst = &mut dst[..width];
    let done = half_leaf!(luma(rows, dst, width, tone));
    let [up, cur, dn] = rows;
    let t = |i: usize| f16::fma(f16::mul(f16::add(up[i], dn[i]), H_QUARTER), cur[i], H_HALF);
    for (x, d) in dst.iter_mut().enumerate().skip(done) {
        let s = f16::fma(
            f16::mul(f16::add(t(x), t(x + 2)), H_QUARTER),
            t(x + 1),
            H_HALF,
        );
        *d = tone.apply(f16::add(s, H_16));
    }
}

/// Half-size luma: each quad's mean through the tone curve.
pub fn quad_luma_row(top: &[u16], bottom: &[u16], dst: &mut [u8], width: usize, tone: &HalfTone) {
    let (top, bottom, dst) = (&top[..2 * width], &bottom[..2 * width], &mut dst[..width]);
    let done = half_leaf!(quad_luma(top, bottom, dst, width, tone));
    for (i, d) in dst.iter_mut().enumerate().skip(done) {
        let s = f16::add(
            f16::add(top[2 * i], top[2 * i + 1]),
            f16::add(bottom[2 * i], bottom[2 * i + 1]),
        );
        *d = tone.apply(f16::fma(H_16, s, H_QUARTER));
    }
}

/// Each quad's R, mean G and B as 12-bit integers (rounded), for the statistics.
pub fn quad_stats_row(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) {
    let (top, bottom) = (&top[..2 * width], &bottom[..2 * width]);
    let [r, g, b] = out;
    let (r, g, b) = (&mut r[..width], &mut g[..width], &mut b[..width]);
    let done = half_leaf!(quad_stats(
        top,
        bottom,
        [&mut *r, &mut *g, &mut *b],
        width,
        pattern
    ));
    let pos = quad_positions(pattern);
    for i in done..width {
        let q = quad(top, bottom, i, pos);
        r[i] = f16::to_u16_round(q[0]);
        g[i] = f16::to_u16_round(q[1]);
        b[i] = f16::to_u16_round(q[2]);
    }
}

#[cfg(test)]
#[path = "half_tests.rs"]
mod tests;
