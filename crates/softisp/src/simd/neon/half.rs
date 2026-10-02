//! FP16 leaves of [`crate::simd::half`] (ARMv8.2 FP16 arithmetic): each must match the
//! scalar oracle bit for bit, and returns the pixels it handled (a multiple of 8 or 16).
//!
//! # Safety
//! Every function needs NEON and FP16 (the dispatcher checks FP16 at run time) and slices
//! sized for `width` pixels as the dispatcher documents.

use std::arch::aarch64::*;

use crate::format::CfaPattern;
use crate::simd::half::{
    ColourCoeffs, ColourOut, H_16, H_HALF, H_MAX, H_QUARTER, HalfTone, LscRow, quad_positions,
};

#[inline(always)]
unsafe fn ld(p: &[u16], i: usize) -> float16x8_t {
    // SAFETY: the callers keep `i + 8` within `p`.
    unsafe { vreinterpretq_f16_u16(vld1q_u16(p.as_ptr().add(i))) }
}

#[inline(always)]
unsafe fn splat(bits: u16) -> float16x8_t {
    unsafe { vreinterpretq_f16_u16(vdupq_n_u16(bits)) }
}

/// `(even, odd)` in every pair of lanes.
#[inline(always)]
unsafe fn pair(even: u16, odd: u16) -> float16x8_t {
    unsafe { vreinterpretq_f16_u32(vdupq_n_u32(u32::from(even) | u32::from(odd) << 16)) }
}

/// The tone curve's tables, held in registers.
struct Tables {
    base: uint8x16x3_t,
    slope: uint8x16x3_t,
}

impl Tables {
    #[inline(always)]
    unsafe fn new(t: &HalfTone) -> Self {
        // SAFETY: both tables hold 48 bytes.
        unsafe {
            Self {
                base: vld1q_u8_x3(t.base.as_ptr()),
                slope: vld1q_u8_x3(t.slope.as_ptr()),
            }
        }
    }

    /// Sixteen outputs for the fp16 values `a` (pixels 0..8) and `b` (8..16), each at least
    /// 16.0.
    #[inline(always)]
    unsafe fn apply(&self, a: float16x8_t, b: float16x8_t) -> uint8x16_t {
        unsafe {
            let (a, b) = (vreinterpretq_u8_f16(a), vreinterpretq_u8_f16(b));
            let idx = vqsubq_u8(vuzp2q_u8(a, b), vdupq_n_u8(HalfTone::FIRST));
            let frac = vuzp1q_u8(a, b);
            let base = vqtbl3q_u8(self.base, idx);
            let slope = vqtbl3q_u8(self.slope, idx);
            let round = vdupq_n_u8(128);
            let lo = vmlal_u8(
                vreinterpretq_u16_u8(vzip1q_u8(round, base)),
                vget_low_u8(slope),
                vget_low_u8(frac),
            );
            let hi = vmlal_high_u8(vreinterpretq_u16_u8(vzip2q_u8(round, base)), slope, frac);
            vuzp2q_u8(vreinterpretq_u8_u16(lo), vreinterpretq_u8_u16(hi))
        }
    }
}

/// Store sixteen RGB pixels at `x`.
#[inline(always)]
unsafe fn put16(out: &mut ColourOut, x: usize, rgb: [uint8x16_t; 3]) {
    // SAFETY: the callers keep `x + 16` within the output row.
    unsafe {
        match out {
            ColourOut::Planes(p) => {
                for (plane, v) in p.iter_mut().zip(rgb) {
                    vst1q_u8(plane.as_mut_ptr().add(x), v);
                }
            }
            ColourOut::Packed(d) => vst3q_u8(
                d.as_mut_ptr().add(3 * x),
                uint8x16x3_t(rgb[0], rgb[1], rgb[2]),
            ),
        }
    }
}

/// # Safety
/// See the module; `row` and the lens shading rows hold `width` samples.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn front(
    row: &mut [u16],
    black: [u16; 2],
    gain: [u16; 2],
    lsc: Option<LscRow>,
    width: usize,
) -> usize {
    let mut x = 0;
    // SAFETY: 8 lanes at `x`, `x + 8 <= width`.
    unsafe {
        let magic = vdupq_n_u16(0x6400);
        let (bl, gn) = (pair(black[0], black[1]), pair(gain[0], gain[1]));
        let (zero, max) = (splat(0), splat(H_MAX));
        let ptr = row.as_mut_ptr();
        let base = |x: usize| {
            let v = vreinterpretq_f16_u16(vorrq_u16(vld1q_u16(ptr.add(x)), magic));
            vmulq_f16(vmaxq_f16(vsubq_f16(v, bl), zero), gn)
        };
        match lsc {
            None => {
                while x + 8 <= width {
                    vst1q_u16(ptr.add(x), vreinterpretq_u16_f16(vminq_f16(base(x), max)));
                    x += 8;
                }
            }
            Some(l) => {
                let t = splat(l.t);
                while x + 8 <= width {
                    let g = vfmaq_f16(ld(l.a, x), ld(l.d, x), t);
                    let y = vminq_f16(vmulq_f16(base(x), g), max);
                    vst1q_u16(ptr.add(x), vreinterpretq_u16_f16(y));
                    x += 8;
                }
            }
        }
    }
    x
}

const RAW10_HIGH: [u8; 16] = [
    0, 0xFF, 1, 0xFF, 2, 0xFF, 3, 0xFF, 5, 0xFF, 6, 0xFF, 7, 0xFF, 8, 0xFF,
];
const RAW10_LOW: [u8; 16] = [
    4, 0xFF, 4, 0xFF, 4, 0xFF, 4, 0xFF, 9, 0xFF, 9, 0xFF, 9, 0xFF, 9, 0xFF,
];
const RAW10_SHIFT: [i16; 8] = [0, -2, -4, -6, 0, -2, -4, -6];

/// # Safety
/// See the module; `src` holds the packed row (`width / 4 * 5` bytes at least), `dst` and the
/// lens shading rows `width` samples.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn front_raw10(
    src: &[u8],
    dst: &mut [u16],
    black: [u16; 2],
    gain: [u16; 2],
    lsc: Option<LscRow>,
    width: usize,
) -> usize {
    let (mut x, mut off) = (0, 0);
    // SAFETY: 16 bytes read at `off` (checked) and 8 samples written at `x`, `x + 8 <= width`.
    unsafe {
        let (hi_t, lo_t) = (vld1q_u8(RAW10_HIGH.as_ptr()), vld1q_u8(RAW10_LOW.as_ptr()));
        let sh = vld1q_s16(RAW10_SHIFT.as_ptr());
        let (three, magic) = (vdupq_n_u16(3), vdupq_n_u16(0x6400));
        let (bl, gn) = (pair(black[0], black[1]), pair(gain[0], gain[1]));
        let (zero, max) = (splat(0), splat(H_MAX));
        let base = |off: usize| {
            let v = vld1q_u8(src.as_ptr().add(off));
            let high = vshlq_n_u16::<2>(vreinterpretq_u16_u8(vqtbl1q_u8(v, hi_t)));
            let low = vandq_u16(
                vshlq_u16(vreinterpretq_u16_u8(vqtbl1q_u8(v, lo_t)), sh),
                three,
            );
            let bits = vorrq_u16(vorrq_u16(high, low), magic);
            vmulq_f16(
                vmaxq_f16(vsubq_f16(vreinterpretq_f16_u16(bits), bl), zero),
                gn,
            )
        };
        let out = dst.as_mut_ptr();
        match lsc {
            None => {
                while x + 8 <= width && off + 16 <= src.len() {
                    let y = vminq_f16(base(off), max);
                    vst1q_u16(out.add(x), vreinterpretq_u16_f16(y));
                    x += 8;
                    off += 10;
                }
            }
            Some(l) => {
                let t = splat(l.t);
                while x + 8 <= width && off + 16 <= src.len() {
                    let g = vfmaq_f16(ld(l.a, x), ld(l.d, x), t);
                    let y = vminq_f16(vmulq_f16(base(off), g), max);
                    vst1q_u16(out.add(x), vreinterpretq_u16_f16(y));
                    x += 8;
                    off += 10;
                }
            }
        }
    }
    x
}

/// The three outputs before the tone curve for the 8 pixels at `x`.
#[inline(always)]
unsafe fn colour8(
    rows: [&[u16]; 3],
    x: usize,
    mask: uint16x8_t,
    c: &[[float16x8_t; 3]; 3],
) -> [float16x8_t; 3] {
    let [up, cur, dn] = rows;
    // SAFETY: the caller keeps `x + 10` within the rows.
    unsafe {
        let sixteen = splat(H_16);
        let hs = vaddq_f16(ld(cur, x), ld(cur, x + 2));
        let vs = vaddq_f16(ld(up, x + 1), ld(dn, x + 1));
        let diag = vaddq_f16(
            vaddq_f16(ld(up, x), ld(up, x + 2)),
            vaddq_f16(ld(dn, x), ld(dn, x + 2)),
        );
        let centre = ld(cur, x + 1);
        let g = vbslq_f16(mask, centre, vaddq_f16(hs, vs));
        let xs = vbslq_f16(mask, hs, centre);
        let ys = vbslq_f16(mask, vs, diag);
        std::array::from_fn(|k| {
            let acc = vfmaq_f16(
                vfmaq_f16(vfmaq_f16(sixteen, g, c[k][0]), xs, c[k][1]),
                ys,
                c[k][2],
            );
            vmaxq_f16(acc, sixteen)
        })
    }
}

/// # Safety
/// See the module; rows hold `width + 2` samples, the output `width` pixels.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn colour(
    rows: [&[u16]; 3],
    out: &mut ColourOut,
    width: usize,
    cc: &ColourCoeffs,
    tone: &HalfTone,
) -> usize {
    let mut x = 0;
    // SAFETY: loads reach `x + 18 <= width + 2` and stores `x + 16 <= width`.
    unsafe {
        let mask = vreinterpretq_u16_f16(if cc.green_even {
            pair(0xFFFF, 0)
        } else {
            pair(0, 0xFFFF)
        });
        let c = cc.c.map(|row| row.map(|v| pair(v[0], v[1])));
        let t = Tables::new(tone);
        while x + 16 <= width {
            let a = colour8(rows, x, mask, &c);
            let b = colour8(rows, x + 8, mask, &c);
            let rgb = std::array::from_fn(|k| t.apply(a[k], b[k]));
            put16(out, x, rgb);
            x += 16;
        }
    }
    x
}

/// R, mean G, B of the 8 quads whose top-left samples are `t` (even, odd) and `b`.
#[inline(always)]
unsafe fn quad8(t: uint16x8x2_t, b: uint16x8x2_t, pos: [usize; 4]) -> [float16x8_t; 3] {
    unsafe {
        let q = [t.0, t.1, b.0, b.1].map(|v| vreinterpretq_f16_u16(v));
        [
            q[pos[0]],
            vmulq_f16(vaddq_f16(q[pos[1]], q[pos[2]]), splat(H_HALF)),
            q[pos[3]],
        ]
    }
}

/// # Safety
/// See the module; inputs hold `2 * width` samples, the output `width` pixels.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn quad_colour(
    top: &[u16],
    bottom: &[u16],
    out: &mut ColourOut,
    width: usize,
    m: &[u16; 9],
    pattern: CfaPattern,
    tone: &HalfTone,
) -> usize {
    let mut i = 0;
    let pos = quad_positions(pattern);
    // SAFETY: 32 samples of each input at `2i` and 16 outputs at `i`, `i + 16 <= width`.
    unsafe {
        let m = m.map(|v| splat(v));
        let sixteen = splat(H_16);
        let t = Tables::new(tone);
        let colour = |q: [float16x8_t; 3]| -> [float16x8_t; 3] {
            std::array::from_fn(|k| {
                let acc = vfmaq_f16(
                    vfmaq_f16(vfmaq_f16(sixteen, q[0], m[3 * k]), q[1], m[3 * k + 1]),
                    q[2],
                    m[3 * k + 2],
                );
                vmaxq_f16(acc, sixteen)
            })
        };
        while i + 16 <= width {
            let a = colour(quad8(
                vld2q_u16(top.as_ptr().add(2 * i)),
                vld2q_u16(bottom.as_ptr().add(2 * i)),
                pos,
            ));
            let b = colour(quad8(
                vld2q_u16(top.as_ptr().add(2 * i + 16)),
                vld2q_u16(bottom.as_ptr().add(2 * i + 16)),
                pos,
            ));
            put16(out, i, std::array::from_fn(|k| t.apply(a[k], b[k])));
            i += 16;
        }
    }
    i
}

/// # Safety
/// See the module; rows hold `width + 2` samples, `dst` `width` bytes.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn luma(
    rows: [&[u16]; 3],
    dst: &mut [u8],
    width: usize,
    tone: &HalfTone,
) -> usize {
    let [up, cur, dn] = rows;
    let mut x = 0;
    // SAFETY: loads reach `x + 18 <= width + 2`, stores `x + 16 <= width`.
    unsafe {
        let (half, quarter, sixteen) = (splat(H_HALF), splat(H_QUARTER), splat(H_16));
        let t = |i: usize| {
            vfmaq_f16(
                vmulq_f16(vaddq_f16(ld(up, i), ld(dn, i)), quarter),
                ld(cur, i),
                half,
            )
        };
        let s = |x: usize| {
            let s = vfmaq_f16(
                vmulq_f16(vaddq_f16(t(x), t(x + 2)), quarter),
                t(x + 1),
                half,
            );
            vaddq_f16(s, sixteen)
        };
        let tables = Tables::new(tone);
        while x + 16 <= width {
            vst1q_u8(dst.as_mut_ptr().add(x), tables.apply(s(x), s(x + 8)));
            x += 16;
        }
    }
    x
}

/// # Safety
/// See the module; inputs hold `2 * width` samples, `dst` `width` bytes.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn quad_luma(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u8],
    width: usize,
    tone: &HalfTone,
) -> usize {
    let mut i = 0;
    // SAFETY: 32 samples of each input at `2i`, 16 outputs at `i`, `i + 16 <= width`.
    unsafe {
        let (quarter, sixteen) = (splat(H_QUARTER), splat(H_16));
        let s = |i: usize| {
            let t = vld2q_u16(top.as_ptr().add(2 * i));
            let b = vld2q_u16(bottom.as_ptr().add(2 * i));
            let f = |v| vreinterpretq_f16_u16(v);
            let sum = vaddq_f16(vaddq_f16(f(t.0), f(t.1)), vaddq_f16(f(b.0), f(b.1)));
            vfmaq_f16(sixteen, sum, quarter)
        };
        let tables = Tables::new(tone);
        while i + 16 <= width {
            vst1q_u8(dst.as_mut_ptr().add(i), tables.apply(s(i), s(i + 8)));
            i += 16;
        }
    }
    i
}

/// # Safety
/// See the module; inputs hold `2 * width` samples, outputs `width`.
#[target_feature(enable = "neon,fp16")]
pub(in crate::simd) unsafe fn quad_stats(
    top: &[u16],
    bottom: &[u16],
    mut out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
    scale: u16,
) -> usize {
    let mut i = 0;
    let pos = quad_positions(pattern);
    // SAFETY: 16 samples of each input at `2i`, 8 outputs at `i`, `i + 8 <= width`.
    unsafe {
        let (scale, max) = (splat(scale), vdupq_n_u16(4095));
        while i + 8 <= width {
            let q = quad8(
                vld2q_u16(top.as_ptr().add(2 * i)),
                vld2q_u16(bottom.as_ptr().add(2 * i)),
                pos,
            );
            for (o, v) in out.iter_mut().zip(q) {
                let v = vminq_u16(vcvtnq_u16_f16(vmulq_f16(v, scale)), max);
                vst1q_u16(o.as_mut_ptr().add(i), v);
            }
            i += 8;
        }
    }
    i
}

/// # Safety
/// NEON (always on AArch64; the conversion is base ARMv8, not FP16 arithmetic); `src` and
/// `dst` hold the same number of elements.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn from_f32(src: &[f32], dst: &mut [u16]) -> usize {
    let mut i = 0;
    // SAFETY: 8 elements of each at `i`, `i + 8 <= len`.
    unsafe {
        while i + 8 <= src.len() {
            let lo = vcvt_f16_f32(vld1q_f32(src.as_ptr().add(i)));
            let v = vcvt_high_f16_f32(lo, vld1q_f32(src.as_ptr().add(i + 4)));
            vst1q_u16(dst.as_mut_ptr().add(i), vreinterpretq_u16_f16(v));
            i += 8;
        }
    }
    i
}
