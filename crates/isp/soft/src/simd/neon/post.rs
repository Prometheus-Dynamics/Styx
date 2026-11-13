//! NEON leaves of the output picture's stages, 8 samples a step in 16-bit lanes.
//!
//! SAFETY (every leaf): NEON is always available on AArch64, and the dispatcher sized the rows
//! as each leaf reads them.

use core::arch::aarch64::*;

use super::{SimdBackend, done};
use crate::simd::post::RCP;

/// The low and high bytes of [`RCP`], as lookup tables.
const fn rcp_bytes(shift: u32) -> [u8; 16] {
    let mut t = [0u8; 16];
    let mut i = 0;
    while i < RCP.len() {
        t[i] = (RCP[i] >> shift) as u8;
        i += 1;
    }
    t
}
const RCP_LO: [u8; 16] = rcp_bytes(0);
const RCP_HI: [u8; 16] = rcp_bytes(8);

#[inline(always)]
unsafe fn widen(v: uint8x8_t) -> int16x8_t {
    unsafe { vreinterpretq_s16_u16(vmovl_u8(v)) }
}

/// `(d k + 128) >> 8` in 16 bits (the result must fit).
#[inline(always)]
unsafe fn scale_q8(d: int16x8_t, k: i16) -> int16x8_t {
    unsafe {
        vcombine_s16(
            vrshrn_n_s32::<8>(vmull_n_s16(vget_low_s16(d), k)),
            vrshrn_n_s32::<8>(vmull_n_s16(vget_high_s16(d), k)),
        )
    }
}

/// `((k R + k G + k B + 128) >> 8) + 128`. The sum reaches 32640 and only the rounding can pass
/// `i16`: saturating it gives 127 + 128 = 255, which the clamp would.
#[inline(always)]
unsafe fn chroma_lane(px: [int16x8_t; 3], k: [i16; 3]) -> uint8x8_t {
    unsafe {
        let s = vaddq_s16(
            vaddq_s16(vmulq_n_s16(px[0], k[0]), vmulq_n_s16(px[1], k[1])),
            vmulq_n_s16(px[2], k[2]),
        );
        let c = vdupq_n_s16(128);
        vqmovun_s16(vaddq_s16(vshrq_n_s16::<8>(vqaddq_s16(s, c)), c))
    }
}

#[target_feature(enable = "neon")]
unsafe fn rgb_to_planes(
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> usize {
    let mut x = 0;
    // SAFETY: 24 bytes read and 8 written per plane at `x`, `x + 8 <= width`.
    unsafe {
        let (kr, kg, kb) = (vdup_n_u8(77), vdup_n_u8(150), vdup_n_u8(29));
        while x + 8 <= width {
            let px = vld3_u8(rgb.as_ptr().add(3 * x));
            let luma = vmlal_u8(vmlal_u8(vmull_u8(px.0, kr), px.1, kg), px.2, kb);
            vst1_u8(y.as_mut_ptr().add(x), vrshrn_n_u16::<8>(luma));
            let l = [widen(px.0), widen(px.1), widen(px.2)];
            vst1_u8(cb.as_mut_ptr().add(x), chroma_lane(l, [-43, -85, 128]));
            vst1_u8(cr.as_mut_ptr().add(x), chroma_lane(l, [128, -107, -21]));
            x += 8;
        }
    }
    x
}

#[target_feature(enable = "neon")]
unsafe fn planes_to_rgb(planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize) -> usize {
    let [r, g, b] = rgb;
    let mut x = 0;
    // SAFETY: 8 bytes of each plane read and written at `x`, `x + 8 <= width`.
    unsafe {
        let mid = vdupq_n_s16(128);
        while x + 8 <= width {
            let y = widen(vld1_u8(planes[0].as_ptr().add(x)));
            let cb = vsubq_s16(widen(vld1_u8(planes[1].as_ptr().add(x))), mid);
            let cr = vsubq_s16(widen(vld1_u8(planes[2].as_ptr().add(x))), mid);
            // `359 = 256 + 103`, `-183 = -256 + 73`, `454 = 512 - 58` keep the products in 16 bits.
            let sr = vshrq_n_s16::<8>(vaddq_s16(vmulq_n_s16(cr, 103), mid));
            let sg = vshrq_n_s16::<8>(vaddq_s16(
                vaddq_s16(vmulq_n_s16(cb, -88), vmulq_n_s16(cr, 73)),
                mid,
            ));
            let sb = vshrq_n_s16::<8>(vaddq_s16(vmulq_n_s16(cb, -58), mid));
            let out = [
                vaddq_s16(vaddq_s16(y, cr), sr),
                vaddq_s16(vsubq_s16(y, cr), sg),
                vaddq_s16(vaddq_s16(y, vaddq_s16(cb, cb)), sb),
            ];
            vst1_u8(r.as_mut_ptr().add(x), vqmovun_s16(out[0]));
            vst1_u8(g.as_mut_ptr().add(x), vqmovun_s16(out[1]));
            vst1_u8(b.as_mut_ptr().add(x), vqmovun_s16(out[2]));
            x += 8;
        }
    }
    x
}

#[target_feature(enable = "neon")]
unsafe fn sigma(rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16) -> usize {
    let width = dst.len();
    let [up, cur, dn] = rows;
    let mut x = 0;
    // SAFETY: 8 bytes of each row at `x` to `x + 2`, `x + 8 <= width`, each row holding
    // `width + 2`; 8 written at `x`.
    unsafe {
        let (lo_table, hi_table) = (vld1q_u8(RCP_LO.as_ptr()), vld1q_u8(RCP_HI.as_ptr()));
        let mut lanes = [0u16; 8];
        while x + 8 <= width {
            for (i, l) in lanes.iter_mut().enumerate() {
                *l = table[usize::from(cur[x + 1 + i])];
            }
            let t = vld1q_u16(lanes.as_ptr());
            let c = vmovl_u8(vld1_u8(cur.as_ptr().add(x + 1)));
            let (mut sum, mut cnt) = (c, vdupq_n_u16(1));
            for (row, at) in [
                (up, 0),
                (up, 1),
                (up, 2),
                (cur, 0),
                (cur, 2),
                (dn, 0),
                (dn, 1),
                (dn, 2),
            ] {
                let v = vmovl_u8(vld1_u8(row.as_ptr().add(x + at)));
                let ok = vcleq_u16(vshlq_n_u16::<2>(vabdq_u16(v, c)), t);
                sum = vaddq_u16(sum, vandq_u16(ok, v));
                cnt = vsubq_u16(cnt, ok);
            }
            // The high byte of a count looks up entry 0, which is 0 in both tables.
            let index = vreinterpretq_u8_u16(cnt);
            let rcp = vorrq_u16(
                vreinterpretq_u16_u8(vqtbl1q_u8(lo_table, index)),
                vshlq_n_u16::<8>(vreinterpretq_u16_u8(vqtbl1q_u8(hi_table, index))),
            );
            let avg = vcombine_u16(
                vrshrn_n_u32::<16>(vmull_u16(vget_low_u16(sum), vget_low_u16(rcp))),
                vrshrn_n_u32::<16>(vmull_u16(vget_high_u16(sum), vget_high_u16(rcp))),
            );
            let c = vreinterpretq_s16_u16(c);
            let d = vsubq_s16(vreinterpretq_s16_u16(avg), c);
            let out = vaddq_s16(c, scale_q8(d, s));
            vst1_u8(dst.as_mut_ptr().add(x), vqmovun_s16(out));
            x += 8;
        }
    }
    x
}

/// `(sum(x) + 2 sum(x + 1) + sum(x + 2) + 8) >> 4` of the vertical `[1 2 1]` sums at the columns
/// from `x`, and the samples of the middle row at `x + 1`.
#[inline(always)]
unsafe fn binomial(rows: [&[u8]; 3], x: usize) -> (int16x8_t, int16x8_t) {
    unsafe {
        let column = |at: usize| {
            let m = vld1_u8(rows[1].as_ptr().add(x + at));
            let ends = vaddl_u8(
                vld1_u8(rows[0].as_ptr().add(x + at)),
                vld1_u8(rows[2].as_ptr().add(x + at)),
            );
            (vaddw_u8(vaddw_u8(ends, m), m), m)
        };
        let (a, _) = column(0);
        let (b, c) = column(1);
        let (d, _) = column(2);
        let total = vaddq_u16(vaddq_u16(a, d), vaddq_u16(b, b));
        (vreinterpretq_s16_u16(vrshrq_n_u16::<4>(total)), widen(c))
    }
}

#[target_feature(enable = "neon")]
unsafe fn chroma(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16) -> usize {
    let width = dst.len();
    let mut x = 0;
    // SAFETY: as in `sigma`.
    unsafe {
        let limit = vdupq_n_s16(t);
        while x + 8 <= width {
            let (low, c) = binomial(rows, x);
            let d = vsubq_s16(low, c);
            let ok = vcleq_s16(vshlq_n_s16::<2>(vabsq_s16(d)), limit);
            let step = vandq_s16(vreinterpretq_s16_u16(ok), scale_q8(d, s));
            vst1_u8(dst.as_mut_ptr().add(x), vqmovun_s16(vaddq_s16(c, step)));
            x += 8;
        }
    }
    x
}

#[target_feature(enable = "neon")]
unsafe fn sharpen(rows: [&[u8]; 3], dst: &mut [u8], amount: i16, thr: i16, limit: i16) -> usize {
    let width = dst.len();
    let mut x = 0;
    // SAFETY: as in `sigma`.
    unsafe {
        let (thr, hi, lo) = (vdupq_n_s16(thr), vdupq_n_s16(limit), vdupq_n_s16(-limit));
        let z = vdupq_n_s16(0);
        while x + 8 <= width {
            let (blur, c) = binomial(rows, x);
            let detail = vsubq_s16(c, blur);
            let cored = vmaxq_s16(vsubq_s16(vabsq_s16(detail), thr), z);
            let signed = vbslq_s16(vcltzq_s16(detail), vnegq_s16(cored), cored);
            let corr = vminq_s16(vmaxq_s16(scale_q8(signed, amount), lo), hi);
            vst1_u8(dst.as_mut_ptr().add(x), vqmovun_s16(vaddq_s16(c, corr)));
            x += 8;
        }
    }
    x
}

pub(in crate::simd) fn rgb_to_planes_row(
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { rgb_to_planes(rgb, y, cb, cr, width) })
}

pub(in crate::simd) fn planes_to_rgb_row(
    planes: [&[u8]; 3],
    rgb: [&mut [u8]; 3],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { planes_to_rgb(planes, rgb, width) })
}

pub(in crate::simd) fn sigma_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    table: &[u16; 256],
    s: i16,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { sigma(rows, dst, table, s) })
}

pub(in crate::simd) fn chroma_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    t: i16,
    s: i16,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { chroma(rows, dst, t, s) })
}

pub(in crate::simd) fn sharpen_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    amount: i16,
    thr: i16,
    limit: i16,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { sharpen(rows, dst, amount, thr, limit) })
}
