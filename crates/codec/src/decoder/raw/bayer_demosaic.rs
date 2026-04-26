use rayon::prelude::*;

use super::{BayerPattern, bayer_unpack::sample_at};

fn color_at(pattern: BayerPattern, x: usize, y: usize) -> (bool, bool, bool) {
    match pattern {
        BayerPattern::RGGB => match (y & 1, x & 1) {
            (0, 0) => (true, false, false),
            (0, 1) => (false, true, false),
            (1, 0) => (false, true, false),
            _ => (false, false, true),
        },
        BayerPattern::BGGR => match (y & 1, x & 1) {
            (0, 0) => (false, false, true),
            (0, 1) => (false, true, false),
            (1, 0) => (false, true, false),
            _ => (true, false, false),
        },
        BayerPattern::GBRG => match (y & 1, x & 1) {
            (0, 0) => (false, true, false),
            (0, 1) => (false, false, true),
            (1, 0) => (true, false, false),
            _ => (false, true, false),
        },
        BayerPattern::GRBG => match (y & 1, x & 1) {
            (0, 0) => (false, true, false),
            (0, 1) => (true, false, false),
            (1, 0) => (false, false, true),
            _ => (false, true, false),
        },
    }
}

#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub(super) fn demosaic_bilinear_to_rg24(
    dst: &mut [u8],
    data: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    pattern: BayerPattern,
    bit_depth: u8,
    bytes_per_sample: usize,
) {
    if bytes_per_sample == 2 && stride.is_multiple_of(2) {
        demosaic_bilinear_u16_le(dst, data, stride / 2, width, height, pattern, bit_depth);
        return;
    }

    for y in 0..height {
        for x in 0..width {
            let (is_r, _is_g, is_b) = color_at(pattern, x, y);
            let center = sample_at(
                data,
                stride,
                bytes_per_sample,
                bit_depth,
                x,
                y,
                width,
                height,
            ) as u16;

            let r;
            let g;
            let b;

            if is_r {
                r = center;
                let g_sum = sample_at(
                    data,
                    stride,
                    bytes_per_sample,
                    bit_depth,
                    x + 1,
                    y,
                    width,
                    height,
                ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x,
                        y + 1,
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y,
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x,
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16;
                g = (g_sum / 4) as u16;
                let b_sum = sample_at(
                    data,
                    stride,
                    bytes_per_sample,
                    bit_depth,
                    x + 1,
                    y + 1,
                    width,
                    height,
                ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y + 1,
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x + 1,
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16;
                b = (b_sum / 4) as u16;
            } else if is_b {
                b = center;
                let g_sum = sample_at(
                    data,
                    stride,
                    bytes_per_sample,
                    bit_depth,
                    x + 1,
                    y,
                    width,
                    height,
                ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x,
                        y + 1,
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y,
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x,
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16;
                g = (g_sum / 4) as u16;
                let r_sum = sample_at(
                    data,
                    stride,
                    bytes_per_sample,
                    bit_depth,
                    x + 1,
                    y + 1,
                    width,
                    height,
                ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y + 1,
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x + 1,
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16
                    + sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16;
                r = (r_sum / 4) as u16;
            } else {
                g = center;
                let on_red_row = match pattern {
                    BayerPattern::RGGB | BayerPattern::GRBG => (y & 1) == 0,
                    BayerPattern::BGGR | BayerPattern::GBRG => (y & 1) == 1,
                };
                let on_red_col = match pattern {
                    BayerPattern::RGGB | BayerPattern::GBRG => (x & 1) == 0,
                    BayerPattern::BGGR | BayerPattern::GRBG => (x & 1) == 1,
                };
                if on_red_row == on_red_col {
                    r = ((sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y,
                        width,
                        height,
                    ) as u16
                        + sample_at(
                            data,
                            stride,
                            bytes_per_sample,
                            bit_depth,
                            x + 1,
                            y,
                            width,
                            height,
                        ) as u16)
                        / 2) as u16;
                    b = ((sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x,
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16
                        + sample_at(
                            data,
                            stride,
                            bytes_per_sample,
                            bit_depth,
                            x,
                            y + 1,
                            width,
                            height,
                        ) as u16)
                        / 2) as u16;
                } else {
                    r = ((sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x,
                        y.saturating_sub(1),
                        width,
                        height,
                    ) as u16
                        + sample_at(
                            data,
                            stride,
                            bytes_per_sample,
                            bit_depth,
                            x,
                            y + 1,
                            width,
                            height,
                        ) as u16)
                        / 2) as u16;
                    b = ((sample_at(
                        data,
                        stride,
                        bytes_per_sample,
                        bit_depth,
                        x.saturating_sub(1),
                        y,
                        width,
                        height,
                    ) as u16
                        + sample_at(
                            data,
                            stride,
                            bytes_per_sample,
                            bit_depth,
                            x + 1,
                            y,
                            width,
                            height,
                        ) as u16)
                        / 2) as u16;
                }
            }

            let dst_idx = (y * width + x) * 3;
            dst[dst_idx] = r as u8;
            dst[dst_idx + 1] = g as u8;
            dst[dst_idx + 2] = b as u8;
        }
    }
}

#[allow(clippy::needless_range_loop)]
pub(super) fn demosaic_bilinear_u16_le(
    dst: &mut [u8],
    data: &[u8],
    stride_px: usize,
    width: usize,
    height: usize,
    pattern: BayerPattern,
    bit_depth: u8,
) {
    let shift = (bit_depth.saturating_sub(8)) as u32;
    let src_u16 = unsafe {
        std::slice::from_raw_parts(
            data.as_ptr() as *const u16,
            stride_px.saturating_mul(height),
        )
    };

    #[inline(always)]
    fn read(src: &[u16], stride_px: usize, x: usize, y: usize) -> u16 {
        u16::from_le(src[y * stride_px + x])
    }

    #[inline(always)]
    fn to_u8(v: u16, shift: u32) -> u8 {
        (v >> shift) as u8
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn avg2_u16(
        a: std::arch::aarch64::uint16x8_t,
        b: std::arch::aarch64::uint16x8_t,
    ) -> std::arch::aarch64::uint16x8_t {
        use std::arch::aarch64::{
            vaddq_u32, vcombine_u16, vget_high_u16, vget_low_u16, vmovl_u16, vmovn_u32, vshrq_n_u32,
        };
        unsafe {
            let a0 = vmovl_u16(vget_low_u16(a));
            let a1 = vmovl_u16(vget_high_u16(a));
            let b0 = vmovl_u16(vget_low_u16(b));
            let b1 = vmovl_u16(vget_high_u16(b));
            let lo = vshrq_n_u32(vaddq_u32(a0, b0), 1);
            let hi = vshrq_n_u32(vaddq_u32(a1, b1), 1);
            vcombine_u16(vmovn_u32(lo), vmovn_u32(hi))
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn avg4_u16(
        a: std::arch::aarch64::uint16x8_t,
        b: std::arch::aarch64::uint16x8_t,
        c: std::arch::aarch64::uint16x8_t,
        d: std::arch::aarch64::uint16x8_t,
    ) -> std::arch::aarch64::uint16x8_t {
        use std::arch::aarch64::{
            vaddq_u32, vcombine_u16, vget_high_u16, vget_low_u16, vmovl_u16, vmovn_u32, vshrq_n_u32,
        };

        unsafe {
            let a0 = vmovl_u16(vget_low_u16(a));
            let a1 = vmovl_u16(vget_high_u16(a));
            let b0 = vmovl_u16(vget_low_u16(b));
            let b1 = vmovl_u16(vget_high_u16(b));
            let c0 = vmovl_u16(vget_low_u16(c));
            let c1 = vmovl_u16(vget_high_u16(c));
            let d0 = vmovl_u16(vget_low_u16(d));
            let d1 = vmovl_u16(vget_high_u16(d));

            let lo = vshrq_n_u32(vaddq_u32(vaddq_u32(a0, b0), vaddq_u32(c0, d0)), 2);
            let hi = vshrq_n_u32(vaddq_u32(vaddq_u32(a1, b1), vaddq_u32(c1, d1)), 2);
            vcombine_u16(vmovn_u32(lo), vmovn_u32(hi))
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn shift_u16x8_to_u8(
        v: std::arch::aarch64::uint16x8_t,
        shift: u32,
    ) -> std::arch::aarch64::uint8x8_t {
        use std::arch::aarch64::{vmovn_u16, vshrn_n_u16};
        unsafe {
            match shift {
                0 => vmovn_u16(v),
                1 => vshrn_n_u16(v, 1),
                2 => vshrn_n_u16(v, 2),
                3 => vshrn_n_u16(v, 3),
                4 => vshrn_n_u16(v, 4),
                5 => vshrn_n_u16(v, 5),
                6 => vshrn_n_u16(v, 6),
                7 => vshrn_n_u16(v, 7),
                8 => vshrn_n_u16(v, 8),
                _ => vshrn_n_u16(v, 8),
            }
        }
    }

    for x in 0..width {
        for y in [0usize, height - 1] {
            let c = to_u8(read(src_u16, stride_px, x, y), shift);
            let o = (y * width + x) * 3;
            dst[o] = c;
            dst[o + 1] = c;
            dst[o + 2] = c;
        }
    }
    for y in 1..(height - 1) {
        for x in [0usize, width - 1] {
            let c = to_u8(read(src_u16, stride_px, x, y), shift);
            let o = (y * width + x) * 3;
            dst[o] = c;
            dst[o + 1] = c;
            dst[o + 2] = c;
        }
    }

    let row_bytes = width * 3;
    let dst_inner = &mut dst[row_bytes..(height - 1) * row_bytes];
    dst_inner
        .par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(row_idx, out_row)| {
            let y = row_idx + 1;
            let ym1 = y - 1;
            let yp1 = y + 1;

            #[cfg(target_arch = "aarch64")]
            unsafe {
                use std::arch::aarch64::{
                    uint8x8x3_t, uint16x8_t, vbslq_u16, vld1q_u16, vmvnq_u16, vst3_u8,
                };

                const MASK_START_EVEN: [u16; 8] = [
                    0xFFFF, 0x0000, 0xFFFF, 0x0000, 0xFFFF, 0x0000, 0xFFFF, 0x0000,
                ];
                let mask_start_even: uint16x8_t = vld1q_u16(MASK_START_EVEN.as_ptr());
                let mask_x_is_even: uint16x8_t = if (1usize & 1) == 0 {
                    mask_start_even
                } else {
                    vmvnq_u16(mask_start_even)
                };

                let row_up = src_u16.as_ptr().add(ym1 * stride_px);
                let row = src_u16.as_ptr().add(y * stride_px);
                let row_dn = src_u16.as_ptr().add(yp1 * stride_px);

                let mut x = 1usize;
                while x + 8 <= width - 1 {
                    let c = vld1q_u16(row.add(x));
                    let l = vld1q_u16(row.add(x - 1));
                    let r = vld1q_u16(row.add(x + 1));
                    let u = vld1q_u16(row_up.add(x));
                    let d = vld1q_u16(row_dn.add(x));
                    let ul = vld1q_u16(row_up.add(x - 1));
                    let ur = vld1q_u16(row_up.add(x + 1));
                    let dl = vld1q_u16(row_dn.add(x - 1));
                    let dr = vld1q_u16(row_dn.add(x + 1));

                    let g_lrud = avg4_u16(l, r, u, d);
                    let diag = avg4_u16(ul, ur, dl, dr);
                    let lr2 = avg2_u16(l, r);
                    let ud2 = avg2_u16(u, d);

                    let y_is_even = (y & 1) == 0;
                    let (r_even, g_even, b_even, r_odd, g_odd, b_odd) = match pattern {
                        BayerPattern::RGGB => {
                            if y_is_even {
                                (c, g_lrud, diag, lr2, c, ud2)
                            } else {
                                (ud2, c, lr2, diag, g_lrud, c)
                            }
                        }
                        BayerPattern::BGGR => {
                            if y_is_even {
                                (diag, g_lrud, c, ud2, c, lr2)
                            } else {
                                (lr2, c, ud2, c, g_lrud, diag)
                            }
                        }
                        BayerPattern::GBRG => {
                            if y_is_even {
                                (ud2, c, lr2, diag, g_lrud, c)
                            } else {
                                (c, g_lrud, diag, lr2, c, ud2)
                            }
                        }
                        BayerPattern::GRBG => {
                            if y_is_even {
                                (lr2, c, ud2, c, g_lrud, diag)
                            } else {
                                (diag, g_lrud, c, ud2, c, lr2)
                            }
                        }
                    };

                    let r16 = vbslq_u16(mask_x_is_even, r_even, r_odd);
                    let g16 = vbslq_u16(mask_x_is_even, g_even, g_odd);
                    let b16 = vbslq_u16(mask_x_is_even, b_even, b_odd);

                    let r8 = shift_u16x8_to_u8(r16, shift);
                    let g8 = shift_u16x8_to_u8(g16, shift);
                    let b8 = shift_u16x8_to_u8(b16, shift);
                    let rgb = uint8x8x3_t(r8, g8, b8);
                    vst3_u8(out_row.as_mut_ptr().add(x * 3), rgb);

                    x += 8;
                }

                for x in x..(width - 1) {
                    let xm1 = x - 1;
                    let xp1 = x + 1;
                    let c = read(src_u16, stride_px, x, y);
                    let l = read(src_u16, stride_px, xm1, y);
                    let r = read(src_u16, stride_px, xp1, y);
                    let u = read(src_u16, stride_px, x, ym1);
                    let d = read(src_u16, stride_px, x, yp1);
                    let ul = read(src_u16, stride_px, xm1, ym1);
                    let ur = read(src_u16, stride_px, xp1, ym1);
                    let dl = read(src_u16, stride_px, xm1, yp1);
                    let dr = read(src_u16, stride_px, xp1, yp1);

                    let (r16, g16, b16) = match pattern {
                        BayerPattern::BGGR => match ((y & 1) == 0, (x & 1) == 0) {
                            (true, true) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (r as u16, g as u16, c)
                            }
                            (true, false) => {
                                let b = (l as u32 + r as u32) / 2;
                                let r = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                            (false, true) => {
                                let r = (l as u32 + r as u32) / 2;
                                let b = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                            (false, false) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (c, g as u16, b as u16)
                            }
                        },
                        BayerPattern::RGGB => match ((y & 1) == 0, (x & 1) == 0) {
                            (true, true) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (c, g as u16, b as u16)
                            }
                            (true, false) => {
                                let r = (l as u32 + r as u32) / 2;
                                let b = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                            (false, true) => {
                                let b = (l as u32 + r as u32) / 2;
                                let r = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                            (false, false) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (r as u16, g as u16, c)
                            }
                        },
                        BayerPattern::GBRG => match ((y & 1) == 0, (x & 1) == 0) {
                            (true, true) => {
                                let r = (u as u32 + d as u32) / 2;
                                let b = (l as u32 + r as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                            (true, false) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (r as u16, g as u16, c)
                            }
                            (false, true) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (c, g as u16, b as u16)
                            }
                            (false, false) => {
                                let r = (l as u32 + r as u32) / 2;
                                let b = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                        },
                        BayerPattern::GRBG => match ((y & 1) == 0, (x & 1) == 0) {
                            (true, true) => {
                                let r = (l as u32 + r as u32) / 2;
                                let b = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                            (true, false) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (c, g as u16, b as u16)
                            }
                            (false, true) => {
                                let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                                let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                                (r as u16, g as u16, c)
                            }
                            (false, false) => {
                                let b = (l as u32 + r as u32) / 2;
                                let r = (u as u32 + d as u32) / 2;
                                (r as u16, c, b as u16)
                            }
                        },
                    };

                    let off = x * 3;
                    out_row[off] = to_u8(r16, shift);
                    out_row[off + 1] = to_u8(g16, shift);
                    out_row[off + 2] = to_u8(b16, shift);
                }
                return;
            }

            #[cfg(not(target_arch = "aarch64"))]
            for x in 1..(width - 1) {
                let xm1 = x - 1;
                let xp1 = x + 1;
                let c = read(src_u16, stride_px, x, y);
                let l = read(src_u16, stride_px, xm1, y);
                let r = read(src_u16, stride_px, xp1, y);
                let u = read(src_u16, stride_px, x, ym1);
                let d = read(src_u16, stride_px, x, yp1);
                let ul = read(src_u16, stride_px, xm1, ym1);
                let ur = read(src_u16, stride_px, xp1, ym1);
                let dl = read(src_u16, stride_px, xm1, yp1);
                let dr = read(src_u16, stride_px, xp1, yp1);

                let (r16, g16, b16) = match pattern {
                    BayerPattern::BGGR => match ((y & 1) == 0, (x & 1) == 0) {
                        (true, true) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (r as u16, g as u16, c)
                        }
                        (true, false) => {
                            let b = (l as u32 + r as u32) / 2;
                            let r = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                        (false, true) => {
                            let r = (l as u32 + r as u32) / 2;
                            let b = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                        (false, false) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (c, g as u16, b as u16)
                        }
                    },
                    BayerPattern::RGGB => match ((y & 1) == 0, (x & 1) == 0) {
                        (true, true) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (c, g as u16, b as u16)
                        }
                        (true, false) => {
                            let r = (l as u32 + r as u32) / 2;
                            let b = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                        (false, true) => {
                            let b = (l as u32 + r as u32) / 2;
                            let r = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                        (false, false) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (r as u16, g as u16, c)
                        }
                    },
                    BayerPattern::GBRG => match ((y & 1) == 0, (x & 1) == 0) {
                        (true, true) => {
                            let r = (u as u32 + d as u32) / 2;
                            let b = (l as u32 + r as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                        (true, false) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (r as u16, g as u16, c)
                        }
                        (false, true) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (c, g as u16, b as u16)
                        }
                        (false, false) => {
                            let r = (l as u32 + r as u32) / 2;
                            let b = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                    },
                    BayerPattern::GRBG => match ((y & 1) == 0, (x & 1) == 0) {
                        (true, true) => {
                            let r = (l as u32 + r as u32) / 2;
                            let b = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                        (true, false) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let b = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (c, g as u16, b as u16)
                        }
                        (false, true) => {
                            let g = (l as u32 + r as u32 + u as u32 + d as u32) / 4;
                            let r = (ul as u32 + ur as u32 + dl as u32 + dr as u32) / 4;
                            (r as u16, g as u16, c)
                        }
                        (false, false) => {
                            let b = (l as u32 + r as u32) / 2;
                            let r = (u as u32 + d as u32) / 2;
                            (r as u16, c, b as u16)
                        }
                    },
                };

                let off = x * 3;
                out_row[off] = to_u8(r16, shift);
                out_row[off + 1] = to_u8(g16, shift);
                out_row[off + 2] = to_u8(b16, shift);
            }
        });
}
