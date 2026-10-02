//! NEON colour-domain kernels: matrix, narrowing, RGB interleave, YCbCr.

use std::arch::aarch64::*;

use crate::simd::YuvCoeffs;

/// # Safety
/// NEON must be available (always on AArch64); planes hold `width` samples.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn ccm(planes: [&mut [u16]; 3], m: &[i16; 9], width: usize) -> usize {
    let [r, g, b] = planes;
    let mut x = 0;
    // SAFETY: 8 lanes of each plane at `x`, `x + 8 <= width`.
    unsafe {
        let (lo, hi) = (vdupq_n_s16(0), vdupq_n_s16(4095));
        while x + 8 <= width {
            let ld =
                |p: &[u16]| vshlq_n_s16::<3>(vreinterpretq_s16_u16(vld1q_u16(p.as_ptr().add(x))));
            let px = [ld(r), ld(g), ld(b)];
            let out = [
                ccm_out(px, &m[0..3], lo, hi),
                ccm_out(px, &m[3..6], lo, hi),
                ccm_out(px, &m[6..9], lo, hi),
            ];
            vst1q_u16(r.as_mut_ptr().add(x), out[0]);
            vst1q_u16(g.as_mut_ptr().add(x), out[1]);
            vst1q_u16(b.as_mut_ptr().add(x), out[2]);
            x += 8;
        }
    }
    x
}

/// One output channel: rounding doubling high-half products, saturating sums, clamped.
#[inline(always)]
unsafe fn ccm_out(px: [int16x8_t; 3], k: &[i16], lo: int16x8_t, hi: int16x8_t) -> uint16x8_t {
    unsafe {
        let s = vqaddq_s16(
            vqaddq_s16(vqrdmulhq_n_s16(px[0], k[0]), vqrdmulhq_n_s16(px[1], k[1])),
            vqrdmulhq_n_s16(px[2], k[2]),
        );
        vreinterpretq_u16_s16(vminq_s16(vmaxq_s16(s, lo), hi))
    }
}

/// # Safety
/// As [`ccm`]; `src` holds `width` samples, `dst` `width` bytes.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn narrow(src: &[u16], dst: &mut [u8], width: usize) -> usize {
    let mut x = 0;
    // SAFETY: 16 samples read and 16 bytes written at `x`, `x + 16 <= width`.
    unsafe {
        while x + 16 <= width {
            let a = vqshrn_n_u16::<4>(vld1q_u16(src.as_ptr().add(x)));
            let b = vqshrn_n_u16::<4>(vld1q_u16(src.as_ptr().add(x + 8)));
            vst1q_u8(dst.as_mut_ptr().add(x), vcombine_u8(a, b));
            x += 16;
        }
    }
    x
}

/// # Safety
/// As [`ccm`]; planes hold `width` bytes, `dst` `3 * width`.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn interleave_rgb(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let mut x = 0;
    // SAFETY: 16 bytes of each plane and 48 output bytes, `x + 16 <= width`.
    unsafe {
        while x + 16 <= width {
            let v = uint8x16x3_t(
                vld1q_u8(planes[0].as_ptr().add(x)),
                vld1q_u8(planes[1].as_ptr().add(x)),
                vld1q_u8(planes[2].as_ptr().add(x)),
            );
            vst3q_u8(dst.as_mut_ptr().add(3 * x), v);
            x += 16;
        }
    }
    x
}

/// # Safety
/// As [`ccm`]; planes and `dst` hold `width` bytes.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn rgb_to_y(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> usize {
    let mut x = 0;
    // SAFETY: 16 bytes of each plane read and written at `x`, `x + 16 <= width`.
    unsafe {
        let k = c.y.map(|k| vdup_n_u8(k as u8));
        let off = vdup_n_u8(c.y_offset as u8);
        let luma = |r: uint8x8_t, g: uint8x8_t, b: uint8x8_t| {
            let s = vmlal_u8(vmlal_u8(vmull_u8(r, k[0]), g, k[1]), b, k[2]);
            vqadd_u8(vrshrn_n_u16::<8>(s), off)
        };
        while x + 16 <= width {
            let px = planes.map(|p| vld1q_u8(p.as_ptr().add(x)));
            let lo = luma(vget_low_u8(px[0]), vget_low_u8(px[1]), vget_low_u8(px[2]));
            let hi = luma(
                vget_high_u8(px[0]),
                vget_high_u8(px[1]),
                vget_high_u8(px[2]),
            );
            vst1q_u8(dst.as_mut_ptr().add(x), vcombine_u8(lo, hi));
            x += 16;
        }
    }
    x
}

/// # Safety
/// As [`ccm`]; input rows hold `2 * width` bytes, `u` `2 * width` (interleaved) or `width`
/// bytes, `v` `width` bytes unless interleaved.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn rgb_to_uv(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) -> usize {
    let mut i = 0;
    // SAFETY: 16 bytes of each input row at `2i`; 16 or 8 bytes written, `i + 8 <= width`.
    unsafe {
        let c128 = vdupq_n_s16(128);
        let chroma = |k: &[i16; 3], m: [int16x8_t; 3]| {
            let s = vmlaq_n_s16(vmlaq_n_s16(vmulq_n_s16(m[0], k[0]), m[1], k[1]), m[2], k[2]);
            vqmovun_s16(vaddq_s16(vrshrq_n_s16::<7>(s), c128))
        };
        while i + 8 <= width {
            let mean: [int16x8_t; 3] = std::array::from_fn(|ch| {
                let s = vpaddlq_u8(vld1q_u8(top[ch].as_ptr().add(2 * i)));
                let s = vpadalq_u8(s, vld1q_u8(bottom[ch].as_ptr().add(2 * i)));
                vreinterpretq_s16_u16(vrshrq_n_u16::<2>(s))
            });
            let (cu, cv) = (chroma(&c.u, mean), chroma(&c.v, mean));
            if interleaved {
                vst2_u8(u.as_mut_ptr().add(2 * i), uint8x8x2_t(cu, cv));
            } else {
                vst1_u8(u.as_mut_ptr().add(i), cu);
                vst1_u8(v.as_mut_ptr().add(i), cv);
            }
            i += 8;
        }
    }
    i
}

/// Eight table bytes for the clamped samples of `v` (four per 64-bit half), as one word.
#[inline(always)]
pub(in crate::simd) unsafe fn lut8(lut: &[u8; 4096], v: uint16x8_t) -> u64 {
    unsafe {
        let v = vminq_u16(v, vdupq_n_u16(4095));
        let (a, b) = (
            vgetq_lane_u64::<0>(vreinterpretq_u64_u16(v)),
            vgetq_lane_u64::<1>(vreinterpretq_u64_u16(v)),
        );
        let at = |w: u64, k: u32| u64::from(lut[(w >> (16 * k)) as usize & 0xFFF]) << (8 * k);
        let lo = at(a, 0) | at(a, 1) | at(a, 2) | at(a, 3);
        let hi = at(b, 0) | at(b, 1) | at(b, 2) | at(b, 3);
        lo | hi << 32
    }
}

/// # Safety
/// As [`ccm`]; `src` holds `width` samples, `dst` `width` bytes.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn lut(
    src: &[u16],
    dst: &mut [u8],
    lut: &[u8; 4096],
    width: usize,
) -> usize {
    let mut x = 0;
    // SAFETY: 8 samples read and 8 bytes written at `x`, `x + 8 <= width`.
    unsafe {
        while x + 8 <= width {
            let w = lut8(lut, vld1q_u16(src.as_ptr().add(x)));
            std::ptr::write_unaligned(dst.as_mut_ptr().add(x).cast::<u64>(), w.to_le());
            x += 8;
        }
    }
    x
}
