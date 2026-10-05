//! The tone table as a quadratic per half octave in 16-bit fixed point
//! ([`Arithmetic::IntPolyTone`](crate::Arithmetic::IntPolyTone)).
//!
//! A table lookup per sample is all scalar loads (AVX2 has no byte table lookup wider than
//! 16 entries and its gathers are slower than scalar loads on Zen 3; NEON's widest `tbl`
//! takes 64). Quadratics per half octave of `v = x + 32` need only 14 sets of coefficients:
//! for each half, a 16-byte table by octave, looked up with `vpshufb` (one per half and a
//! blend) or `tbl` (two registers). The octave
//! `e` is `floor(log2 v)` (the top one, `[2048, 4128)`, takes the last 32 inputs too), the
//! position within it `t = v 2^(14 - e) - 2^14` (Q14, `0..=16632`), the half
//! `min(t >> 13, 1)`, and with `mulhrs(a, b) = (a b + 2^14) >> 15` (`vpmulhrsw`,
//! `sqrdmulh`) and saturating 16-bit adds:
//!
//! ```text
//! out = clamp(mulhrs(c0 + mulhrs(c1 + mulhrs(c2, t), t), 1024), 0, 255)
//! ```
//!
//! (`c0` in 1/32 codes, `c1` and `c2` scaled to match). Each segment's quadratic is fitted to
//! the table (least squares, then reweighted towards the largest misses where those exceed a
//! code) and the set is used only if every one of the 4096 inputs lands within one code of
//! the table. The sRGB, gamma 1.8-2.2 and Raspberry Pi contrast curves do (one sample in six or
//! seven is off by one: the table's own rounding), and so do the adaptive contrast curves of a
//! recorded OV9782 session; steeper ones (gamma 3) do not and keep the table. Whole octaves
//! missed by two codes on most of those adaptive curves.

use alloc::vec;

use super::scalar::WORK_MAX;

#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

/// Added to the input so that its octaves `[2^5, 2^12)` cover `0..=4095`.
const OFFSET: u16 = 32;
/// Half octaves: `[32, 48)`, `[48, 64)` .. `[2048, 3072)`, `[3072, 4128)`.
pub const SEGMENTS: usize = 14;

/// A tone table as 14 quadratics in 16-bit fixed point, one per half octave of `x + 32`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolyTone {
    /// `c[k][h][o]`: coefficient `k` of half `h` of octave `o` (segment `2 o + h`, see
    /// [`segment`]); the eighth entries pad the tables to 16 bytes.
    pub c: [[[i16; 8]; 2]; 3],
}

/// `(a b + 2^14) >> 15`: `vpmulhrsw`, `sqrdmulh` (`t` is never negative, so neither of
/// their edge cases, `-32768 * -32768`, occurs).
#[inline(always)]
fn mulhrs(a: i16, b: i16) -> i16 {
    ((i32::from(a) * i32::from(b) + (1 << 14)) >> 15) as i16
}

/// The segment (`2 octave + half`) and the Q14 position within the octave of working value `x`.
#[inline(always)]
pub fn segment(x: u16) -> (usize, i16) {
    let v = x.min(WORK_MAX) + OFFSET;
    let e = (15 - v.leading_zeros()).min(11);
    let t = (u32::from(v) << (14 - e)) - (1 << 14);
    (2 * (e as usize - 5) + (t >> 13).min(1) as usize, t as i16)
}

impl PolyTone {
    /// The quadratics of `table` (4096 entries, inputs above 4095 clamp), or `None` when they
    /// would miss it by more than one code anywhere.
    pub fn fit(table: &[u8; 4096]) -> Option<Self> {
        let mut c = [[[0i16; 8]; 2]; 3];
        let mut weight = vec![1.0f64; 4096];
        let mut x0 = 0u16;
        for s in 0..SEGMENTS {
            let mut x1 = x0;
            while x1 <= WORK_MAX && segment(x1).0 == s {
                x1 += 1;
            }
            let xs = x0..x1;
            x0 = x1;
            // Least squares, then up to 8 rounds of Lawson's reweighting (towards the minimax
            // fit) while the segment misses by more than a code.
            let mut best: Option<(i32, [i16; 3])> = None;
            for _ in 0..9 {
                let mut m = [[0.0f64; 3]; 3];
                let mut r = [0.0f64; 3];
                for x in xs.clone() {
                    let t = f64::from(segment(x).1) / 16384.0;
                    let p = [1.0, t, t * t];
                    let (w, y) = (weight[x as usize], f64::from(table[x as usize]));
                    for i in 0..3 {
                        for j in 0..3 {
                            m[i][j] += w * p[i] * p[j];
                        }
                        r[i] += w * p[i] * y;
                    }
                }
                let [a, b, q] = solve(m, r)?;
                // y (1/32 codes) = c0 + (c1 + c2 t / 2) t / 2 with t in Q14 read as t / 2.
                let mut k = [0i16; 3];
                for (k, v) in k.iter_mut().zip([32.0 * a, 64.0 * b, 128.0 * q]) {
                    let v = v.round();
                    if !(-32768.0..=32767.0).contains(&v) {
                        return None;
                    }
                    *k = v as i16;
                }
                let miss = |x: u16| (i32::from(apply(&k, x)) - i32::from(table[x as usize])).abs();
                let worst = xs.clone().map(miss).max().unwrap_or(0);
                if best.is_none_or(|(w, _)| worst < w) {
                    best = Some((worst, k));
                }
                if worst <= 1 {
                    break;
                }
                let mut sum = 0.0;
                for x in xs.clone() {
                    let y = f64::from(apply(&k, x)) - f64::from(table[x as usize]);
                    weight[x as usize] *= y.abs() + 0.1;
                    sum += weight[x as usize];
                }
                for x in xs.clone() {
                    weight[x as usize] /= sum;
                }
            }
            let (worst, k) = best?;
            if worst > 1 {
                return None;
            }
            for (i, k) in k.into_iter().enumerate() {
                c[i][s % 2][s / 2] = k;
            }
        }
        Some(Self { c })
    }

    /// The output for working value `x` (clamped to 4095), as the vector kernels compute it.
    #[inline]
    pub fn apply(&self, x: u16) -> u8 {
        let (h, o) = (segment(x).0 % 2, segment(x).0 / 2);
        apply(&[self.c[0][h][o], self.c[1][h][o], self.c[2][h][o]], x)
    }
}

/// One segment's quadratic `k` at `x` (in that segment).
#[inline(always)]
fn apply(k: &[i16; 3], x: u16) -> u8 {
    let t = segment(x).1;
    let inner = k[1].saturating_add(mulhrs(k[2], t));
    let y = k[0].saturating_add(mulhrs(inner, t));
    mulhrs(y, 1024).clamp(0, 255) as u8
}

/// `m x = r` (Gaussian elimination with partial pivoting); `None` when singular.
fn solve(mut m: [[f64; 3]; 3], mut r: [f64; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let pivot = (col..3).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-9 {
            return None;
        }
        m.swap(col, pivot);
        r.swap(col, pivot);
        let (pivot_row, pivot_r) = (m[col], r[col]);
        for (row, ri) in m.iter_mut().zip(r.iter_mut()).skip(col + 1) {
            let f = row[col] / pivot_row[col];
            for (a, b) in row.iter_mut().zip(&pivot_row).skip(col) {
                *a -= f * b;
            }
            *ri -= f * pivot_r;
        }
    }
    let mut x = [0.0f64; 3];
    for row in (0..3).rev() {
        let s: f64 = (row + 1..3).map(|k| m[row][k] * x[k]).sum();
        x[row] = (r[row] - s) / m[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// See [`PolyTone`]: `dst[x] = poly(src[x])`.
pub fn poly_row(src: &[u16], dst: &mut [u8], poly: &PolyTone, width: usize) {
    for (d, &s) in dst[..width].iter_mut().zip(&src[..width]) {
        *d = poly.apply(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToneCurve;
    use crate::simd::ToneLut;

    fn table(curve: &ToneCurve) -> ToneLut {
        ToneLut::from_curve(|x| curve.eval(x))
    }

    /// The Raspberry Pi contrast curve of the OV9782 tuning.
    fn pi_curve() -> ToneCurve {
        let p = [
            0, 0, 512, 2518, 1024, 5033, 1536, 7175, 2048, 9309, 2560, 10814, 3072, 12312, 3584,
            13773, 4096, 15225, 4608, 16566, 5120, 17899, 5632, 19221, 6144, 20534, 6656, 21684,
            7168, 22826, 7680, 24024, 8192, 25212, 9216, 27251, 10240, 29167, 11264, 30947, 12288,
            32696, 13312, 34309, 14336, 35849, 15360, 37194, 16384, 38445, 17408, 39598, 18432,
            40732, 19456, 41717, 20480, 42687, 22528, 44343, 24576, 45871, 26624, 47222, 28672,
            48441, 30720, 49460, 32768, 50470, 34816, 51476, 36864, 52480, 38912, 53382, 40960,
            54294, 43008, 55155, 45056, 56035, 47104, 56920, 49152, 57824, 51200, 58737, 53248,
            59666, 55296, 60604, 57344, 61558, 59392, 62529, 61440, 63516, 63488, 64519, 65535,
            65535,
        ];
        ToneCurve::Points {
            points: p
                .chunks(2)
                .map(|c| [c[0] as f32 / 65535.0, c[1] as f32 / 65535.0])
                .collect(),
        }
    }

    #[test]
    fn common_curves_fit_within_a_code() {
        for curve in [
            ToneCurve::Srgb,
            ToneCurve::Gamma { gamma: 2.2 },
            ToneCurve::Gamma { gamma: 1.8 },
            ToneCurve::Gamma { gamma: 1.0 },
            pi_curve(),
        ] {
            let lut = table(&curve);
            let poly = PolyTone::fit(lut.full()).unwrap_or_else(|| panic!("{curve:?}"));
            let off = (0..=WORK_MAX)
                .filter(|&x| poly.apply(x) != lut.full()[x as usize])
                .count();
            // Mostly exact: the differences are the table's own rounding.
            assert!(off < 4096 / 4, "{curve:?}: {off} inputs off by one");
            assert_eq!(poly.apply(u16::MAX), poly.apply(WORK_MAX));
        }
    }

    #[test]
    fn segments() {
        assert_eq!(segment(0), (0, 0));
        assert_eq!(segment(15), (0, 15 << 9));
        assert_eq!(segment(16), (1, 8192));
        assert_eq!(segment(31), (1, 31 << 9));
        assert_eq!(segment(32), (2, 0));
        assert_eq!(segment(2015), (11, 16384 - 16));
        assert_eq!(segment(2016), (12, 0));
        assert_eq!(segment(3040), (13, 8192));
        assert_eq!(segment(4095), (13, 16632));
        assert_eq!(segment(u16::MAX), segment(4095));
    }

    #[test]
    fn rough_tables_do_not_fit() {
        let nodes = core::array::from_fn(|i| if i % 2 == 0 { 0 } else { 255 });
        assert!(PolyTone::fit(ToneLut::from_nodes(nodes).full()).is_none());
    }
}
