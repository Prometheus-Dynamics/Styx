//! NEON mosaic-domain kernels: unpacking, front end, demosaic, luma, quads. Each processes
//! whole vectors from the start of the row and returns the pixels it wrote.

use std::arch::aarch64::*;

use crate::format::CfaPattern;
use crate::simd::RowKind;

#[inline(always)]
unsafe fn ld(p: &[u16], i: usize) -> uint16x8_t {
    // SAFETY: the callers keep `i + 8` within `p`.
    unsafe { vld1q_u16(p.as_ptr().add(i)) }
}

#[inline(always)]
unsafe fn st(p: &mut [u16], i: usize, v: uint16x8_t) {
    // SAFETY: the callers keep `i + 8` within `p`.
    unsafe { vst1q_u16(p.as_mut_ptr().add(i), v) }
}

/// A 16-bit pair `(lo, hi)` in every 32-bit lane.
#[inline(always)]
unsafe fn pair(lo: u16, hi: u16) -> uint16x8_t {
    unsafe { vreinterpretq_u16_u32(vdupq_n_u32(lo as u32 | (hi as u32) << 16)) }
}

#[inline(always)]
unsafe fn green_mask(kind: RowKind) -> uint16x8_t {
    unsafe {
        if kind.green_even {
            pair(0xFFFF, 0)
        } else {
            pair(0, 0xFFFF)
        }
    }
}

const RAW10_HIGH: [u8; 16] = [
    0, 0xFF, 1, 0xFF, 2, 0xFF, 3, 0xFF, 5, 0xFF, 6, 0xFF, 7, 0xFF, 8, 0xFF,
];
const RAW10_LOW: [u8; 16] = [
    4, 0xFF, 4, 0xFF, 4, 0xFF, 4, 0xFF, 9, 0xFF, 9, 0xFF, 9, 0xFF, 9, 0xFF,
];
const RAW10_SHIFT: [i16; 8] = [0, -2, -4, -6, 0, -2, -4, -6];
const RAW12_HIGH: [u8; 16] = [
    0, 0xFF, 1, 0xFF, 3, 0xFF, 4, 0xFF, 6, 0xFF, 7, 0xFF, 9, 0xFF, 10, 0xFF,
];
const RAW12_LOW: [u8; 16] = [
    2, 0xFF, 2, 0xFF, 5, 0xFF, 5, 0xFF, 8, 0xFF, 8, 0xFF, 11, 0xFF, 11, 0xFF,
];
const RAW12_SHIFT: [i16; 8] = [0, -4, 0, -4, 0, -4, 0, -4];

/// # Safety
/// NEON must be available (always on AArch64); `dst` holds `width` samples.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn unpack(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
    raw12: bool,
) -> usize {
    let (step, high_bits, hi_t, lo_t, sh, low_mask) = if raw12 {
        (12, 4, &RAW12_HIGH, &RAW12_LOW, &RAW12_SHIFT, 0xF)
    } else {
        (10, 2, &RAW10_HIGH, &RAW10_LOW, &RAW10_SHIFT, 3)
    };
    let (mut x, mut off) = (0, 0);
    // SAFETY: 16 bytes read at `off` and 8 samples written at `x`, both checked.
    unsafe {
        let (hi_t, lo_t) = (vld1q_u8(hi_t.as_ptr()), vld1q_u8(lo_t.as_ptr()));
        let sh = vld1q_s16(sh.as_ptr());
        let high_sh = vdupq_n_s16(high_bits);
        let mask = vdupq_n_u16(low_mask);
        while x + 8 <= width && off + 16 <= src.len() {
            let v = vld1q_u8(src.as_ptr().add(off));
            let high = vshlq_u16(vreinterpretq_u16_u8(vqtbl1q_u8(v, hi_t)), high_sh);
            let low = vshlq_u16(vreinterpretq_u16_u8(vqtbl1q_u8(v, lo_t)), sh);
            st(dst, x, vorrq_u16(high, vandq_u16(low, mask)));
            x += 8;
            off += step;
        }
    }
    x
}

/// # Safety
/// As [`unpack`]; `row` and `gains` hold `width` samples.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn front(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> usize {
    let mut x = 0;
    // SAFETY: 8 lanes at `x`, `x + 8 <= width`.
    unsafe {
        let bl = pair(black[0], black[1]);
        let sh = vdupq_n_s16(shift as i16);
        let max = vdupq_n_u16(4095);
        while x + 8 <= width {
            let v = vshlq_u16(vqsubq_u16(ld(row, x), bl), sh);
            let g = ld(gains, x);
            let lo = vmull_u16(vget_low_u16(v), vget_low_u16(g));
            let hi = vmull_high_u16(v, g);
            let v = vcombine_u16(vshrn_n_u32::<16>(lo), vshrn_n_u32::<16>(hi));
            st(row, x, vminq_u16(v, max));
            x += 8;
        }
    }
    x
}

#[inline(always)]
unsafe fn store_rgb(
    out: &mut [&mut [u16]; 3],
    x: usize,
    kind: RowKind,
    g: uint16x8_t,
    xv: uint16x8_t,
    yv: uint16x8_t,
) {
    let (r, b) = if kind.x_is_red { (xv, yv) } else { (yv, xv) };
    // SAFETY: the callers keep `x + 8` within the output rows.
    unsafe {
        st(out[0], x, r);
        st(out[1], x, g);
        st(out[2], x, b);
    }
}

/// # Safety
/// As [`unpack`]; input rows hold `width + 2` samples, outputs `width`.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn demosaic_bilinear(
    rows: [&[u16]; 3],
    mut out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> usize {
    let [up, cur, dn] = rows;
    let mut x = 0;
    // SAFETY: loads at `x + 2` of 8 lanes stay inside the `width + 2` rows while `x + 8 <= width`.
    unsafe {
        let mask = green_mask(kind);
        while x + 8 <= width {
            let (l, c, r) = (ld(cur, x), ld(cur, x + 1), ld(cur, x + 2));
            let (u, d) = (ld(up, x + 1), ld(dn, x + 1));
            let h = vaddq_u16(l, r);
            let v = vaddq_u16(u, d);
            let diag = vaddq_u16(
                vaddq_u16(ld(up, x), ld(up, x + 2)),
                vaddq_u16(ld(dn, x), ld(dn, x + 2)),
            );
            let g_cross = vrshrq_n_u16::<2>(vaddq_u16(h, v));
            let g = vbslq_u16(mask, c, g_cross);
            let xv = vbslq_u16(mask, vrhaddq_u16(l, r), c);
            let yv = vbslq_u16(mask, vrhaddq_u16(u, d), vrshrq_n_u16::<2>(diag));
            store_rgb(&mut out, x, kind, g, xv, yv);
            x += 8;
        }
    }
    x
}

/// `clamp((a0 wa0 + a1 wa1 + b0 wb0 + b1 wb1 + 8) >> 4, 0, 4095)` in 32 bits.
#[inline(always)]
unsafe fn mix(a: [uint16x8_t; 2], wa: [i16; 2], b: [uint16x8_t; 2], wb: [i16; 2]) -> uint16x8_t {
    unsafe {
        let s = |v: uint16x8_t| vreinterpretq_s16_u16(v);
        let half = |lo: bool| {
            let pick = |v: uint16x8_t| {
                if lo {
                    vget_low_s16(s(v))
                } else {
                    vget_high_s16(s(v))
                }
            };
            let acc = vmull_n_s16(pick(a[0]), wa[0]);
            let acc = vmlal_n_s16(acc, pick(a[1]), wa[1]);
            let acc = vmlal_n_s16(acc, pick(b[0]), wb[0]);
            let acc = vmlal_n_s16(acc, pick(b[1]), wb[1]);
            vqrshrun_n_s32::<4>(acc)
        };
        vminq_u16(vcombine_u16(half(true), half(false)), vdupq_n_u16(4095))
    }
}

/// # Safety
/// As [`unpack`]; input rows hold `width + 4` samples, outputs `width`.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn demosaic_mhc(
    rows: [&[u16]; 5],
    mut out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> usize {
    let [u2, u1, c0, d1, d2] = rows;
    let mut x = 0;
    // SAFETY: loads at `x + 4` of 8 lanes stay inside the `width + 4` rows while `x + 8 <= width`.
    unsafe {
        let mask = green_mask(kind);
        let zero = vdupq_n_u16(0);
        while x + 8 <= width {
            let c = ld(c0, x + 2);
            let far_h = vaddq_u16(ld(c0, x), ld(c0, x + 4));
            let far_v = vaddq_u16(ld(u2, x + 2), ld(d2, x + 2));
            let near_h = vaddq_u16(ld(c0, x + 1), ld(c0, x + 3));
            let near_v = vaddq_u16(ld(u1, x + 2), ld(d1, x + 2));
            let diag = vaddq_u16(
                vaddq_u16(ld(u1, x + 1), ld(u1, x + 3)),
                vaddq_u16(ld(d1, x + 1), ld(d1, x + 3)),
            );
            let xs = mix(
                [c, near_h],
                [10, 8],
                [vaddq_u16(far_h, diag), far_v],
                [-2, 1],
            );
            let ys = mix(
                [c, near_v],
                [10, 8],
                [vaddq_u16(far_v, diag), far_h],
                [-2, 1],
            );
            let far = vaddq_u16(far_h, far_v);
            let gs = mix([c, vaddq_u16(near_h, near_v)], [8, 4], [far, zero], [-2, 0]);
            let y2 = mix([c, diag], [12, 4], [far, zero], [-3, 0]);
            let g = vbslq_u16(mask, c, gs);
            let xv = vbslq_u16(mask, xs, c);
            let yv = vbslq_u16(mask, ys, y2);
            store_rgb(&mut out, x, kind, g, xv, yv);
            x += 8;
        }
    }
    x
}

/// # Safety
/// As [`demosaic_bilinear`], with one output row.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn bayer_luma(
    rows: [&[u16]; 3],
    dst: &mut [u16],
    width: usize,
) -> usize {
    let [up, cur, dn] = rows;
    let mut x = 0;
    // SAFETY: as `demosaic_bilinear`.
    unsafe {
        let vs = |i: usize| {
            vaddq_u16(
                vaddq_u16(ld(up, i), ld(dn, i)),
                vshlq_n_u16::<1>(ld(cur, i)),
            )
        };
        while x + 8 <= width {
            let s = vaddq_u16(vaddq_u16(vs(x), vs(x + 2)), vshlq_n_u16::<1>(vs(x + 1)));
            st(dst, x, vrshrq_n_u16::<4>(s));
            x += 8;
        }
    }
    x
}

/// # Safety
/// As [`unpack`]; inputs hold `2 * width` samples, outputs `width`.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn quad_rgb(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> usize {
    let [r, g, b] = out;
    let mut i = 0;
    // SAFETY: 16 samples of each input at `2i` and 8 outputs at `i`, `i + 8 <= width`.
    unsafe {
        while i + 8 <= width {
            let t = vld2q_u16(top.as_ptr().add(2 * i));
            let bt = vld2q_u16(bottom.as_ptr().add(2 * i));
            let (te, to, be, bo) = (t.0, t.1, bt.0, bt.1);
            let (rv, gv, bv) = match pattern {
                CfaPattern::Rggb => (te, vrhaddq_u16(to, be), bo),
                CfaPattern::Bggr => (bo, vrhaddq_u16(to, be), te),
                CfaPattern::Grbg => (to, vrhaddq_u16(te, bo), be),
                CfaPattern::Gbrg => (be, vrhaddq_u16(te, bo), to),
            };
            st(r, i, rv);
            st(g, i, gv);
            st(b, i, bv);
            i += 8;
        }
    }
    i
}

/// # Safety
/// As [`quad_rgb`], with one output row.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn quad_luma(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u16],
    width: usize,
) -> usize {
    let mut i = 0;
    // SAFETY: as `quad_rgb`.
    unsafe {
        while i + 8 <= width {
            let t = vld2q_u16(top.as_ptr().add(2 * i));
            let b = vld2q_u16(bottom.as_ptr().add(2 * i));
            let s = vaddq_u16(vaddq_u16(t.0, t.1), vaddq_u16(b.0, b.1));
            st(dst, i, vrshrq_n_u16::<2>(s));
            i += 8;
        }
    }
    i
}
