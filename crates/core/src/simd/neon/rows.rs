//! NEON row kernels. Each processes whole blocks of pixels from the start of the row and
//! returns how many pixels it wrote; slices must hold `width` pixels.

use std::arch::aarch64::*;

/// Reverse the 16 bytes of `v`.
#[inline(always)]
unsafe fn rev_u8x16(v: uint8x16_t) -> uint8x16_t {
    unsafe {
        let r = vrev64q_u8(v);
        vextq_u8::<8>(r, r)
    }
}

/// # Safety
/// NEON must be available (always on AArch64); `src` and `dst` hold `width` 3-byte pixels.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn swap_rb24(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: block `i` covers pixels 16i..16i+16, inside both rows.
        unsafe {
            let v = vld3q_u8(src.as_ptr().add(i * 48));
            vst3q_u8(dst.as_mut_ptr().add(i * 48), uint8x16x3_t(v.2, v.1, v.0));
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`], with 4-byte pixels in both rows.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn swap_rb32(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row.
        unsafe {
            let v = vld4q_u8(src.as_ptr().add(i * 64));
            vst4q_u8(
                dst.as_mut_ptr().add(i * 64),
                uint8x16x4_t(v.2, v.1, v.0, v.3),
            );
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` 4-byte pixels.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn x32_to_rgb24(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row.
        unsafe {
            let v = vld4q_u8(src.as_ptr().add(i * 64));
            let rgb = if swap {
                uint8x16x3_t(v.2, v.1, v.0)
            } else {
                uint8x16x3_t(v.0, v.1, v.2)
            };
            vst3q_u8(dst.as_mut_ptr().add(i * 48), rgb);
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` pixels of `layout`, `dst` `width` bytes.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn rgb_to_luma(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: super::super::ColorLayout,
) -> usize {
    use super::super::ColorLayout::*;
    let blocks = width / 16;
    let bpp = layout.bytes_per_pixel();
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row. 77 + 150 + 29 = 256, so the sums fit 16 bits.
        unsafe {
            let p = src.as_ptr().add(i * 16 * bpp);
            let (r, g, b) = match layout {
                Rgb24 => {
                    let v = vld3q_u8(p);
                    (v.0, v.1, v.2)
                }
                Bgr24 => {
                    let v = vld3q_u8(p);
                    (v.2, v.1, v.0)
                }
                Rgba32 => {
                    let v = vld4q_u8(p);
                    (v.0, v.1, v.2)
                }
                Bgra32 => {
                    let v = vld4q_u8(p);
                    (v.2, v.1, v.0)
                }
            };
            let (wr, wg, wb) = (vdup_n_u8(77), vdup_n_u8(150), vdup_n_u8(29));
            let lo = vmlal_u8(
                vmlal_u8(vmull_u8(vget_low_u8(r), wr), vget_low_u8(g), wg),
                vget_low_u8(b),
                wb,
            );
            let hi = vmlal_u8(
                vmlal_u8(vmull_u8(vget_high_u8(r), wr), vget_high_u8(g), wg),
                vget_high_u8(b),
                wb,
            );
            vst1q_u8(
                dst.as_mut_ptr().add(i * 16),
                vcombine_u8(vshrn_n_u16::<8>(lo), vshrn_n_u16::<8>(hi)),
            );
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` 3-byte pixels, `dst` `width` 4-byte pixels.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn rgb24_to_rgba(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row.
        unsafe {
            let v = vld3q_u8(src.as_ptr().add(i * 48));
            vst4q_u8(
                dst.as_mut_ptr().add(i * 64),
                uint8x16x4_t(v.0, v.1, v.2, vdupq_n_u8(255)),
            );
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` bytes.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn gray8_to_rgb24(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row.
        unsafe {
            let g = vld1q_u8(src.as_ptr().add(i * 16));
            vst3q_u8(dst.as_mut_ptr().add(i * 48), uint8x16x3_t(g, g, g));
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` 2-byte pixels.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn gray16le_to_rgb24(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row; `.1` holds the high (odd) bytes.
        unsafe {
            let g = vld2q_u8(src.as_ptr().add(i * 32)).1;
            vst3q_u8(dst.as_mut_ptr().add(i * 48), uint8x16x3_t(g, g, g));
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` 6-byte pixels.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn rgb48le_to_rgb24(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels (96 source bytes) of each row. The odd bytes of the three 32-byte
        // loads are the channels' high bytes, already in RGB24 order.
        unsafe {
            let p = src.as_ptr().add(i * 96);
            let hi = [vld2q_u8(p).1, vld2q_u8(p.add(32)).1, vld2q_u8(p.add(64)).1];
            let out = dst.as_mut_ptr().add(i * 48);
            if swap {
                let mut rgb = [0u8; 48];
                vst1q_u8_x3(rgb.as_mut_ptr(), uint8x16x3_t(hi[0], hi[1], hi[2]));
                let v = vld3q_u8(rgb.as_ptr());
                vst3q_u8(out, uint8x16x3_t(v.2, v.1, v.0));
            } else {
                vst1q_u8_x3(out, uint8x16x3_t(hi[0], hi[1], hi[2]));
            }
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `src` holds `width` 2-byte pixels.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn yuyv_luma(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row.
        unsafe {
            let yuv = vld2q_u8(src.as_ptr().add(i * 32));
            vst1q_u8(dst.as_mut_ptr().add(i * 16), yuv.0);
        }
    }
    blocks * 16
}

/// # Safety
/// As [`swap_rb24`]; `top` and `bottom` hold `2 * width` bytes, `dst` `width`.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn box2(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 32 input bytes of each row, 16 outputs. Pairwise sums of the top row plus the
        // bottom row's, then (sum + 2) >> 2.
        unsafe {
            let (t, b) = (top.as_ptr().add(i * 32), bottom.as_ptr().add(i * 32));
            let s0 = vpadalq_u8(vpaddlq_u8(vld1q_u8(t)), vld1q_u8(b));
            let s1 = vpadalq_u8(vpaddlq_u8(vld1q_u8(t.add(16))), vld1q_u8(b.add(16)));
            vst1q_u8(
                dst.as_mut_ptr().add(i * 16),
                vcombine_u8(vrshrn_n_u16::<2>(s0), vrshrn_n_u16::<2>(s1)),
            );
        }
    }
    blocks * 16
}

/// Reverse whole blocks from the start of the source row into the end of `dst`; returns the
/// source pixels done.
///
/// # Safety
/// As [`swap_rb24`]; both rows hold `width` pixels of `bpp` bytes.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn reverse(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
) -> usize {
    let blocks = width / 16;
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // Source block `i` (pixels 16i..16i+16) lands at destination pixels width-16(i+1).. .
    let out = |i: usize| (width - 16 * (i + 1)) * bpp;
    for i in 0..blocks {
        // SAFETY: 16 pixels of each row; the destination block is inside the row.
        unsafe {
            match bpp {
                1 => vst1q_u8(d.add(out(i)), rev_u8x16(vld1q_u8(s.add(i * 16)))),
                2 => {
                    let v = vld2q_u8(s.add(i * 32));
                    vst2q_u8(d.add(out(i)), uint8x16x2_t(rev_u8x16(v.0), rev_u8x16(v.1)));
                }
                3 => {
                    let v = vld3q_u8(s.add(i * 48));
                    let r = uint8x16x3_t(rev_u8x16(v.0), rev_u8x16(v.1), rev_u8x16(v.2));
                    vst3q_u8(d.add(out(i)), r);
                }
                _ => {
                    let v = vld4q_u8(s.add(i * 64));
                    let r = uint8x16x4_t(
                        rev_u8x16(v.0),
                        rev_u8x16(v.1),
                        rev_u8x16(v.2),
                        rev_u8x16(v.3),
                    );
                    vst4q_u8(d.add(out(i)), r);
                }
            }
        }
    }
    blocks * 16
}
