use std::sync::Arc;
use styx_core::prelude::*;
use rayon::prelude::*;

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
enum BayerPattern {
    RGGB,
    BGGR,
    GBRG,
    GRBG,
}

#[derive(Clone, Copy)]
pub struct BayerInfo {
    pattern: BayerPattern,
    bit_depth: u8,
    bytes_per_sample: usize,
}

pub fn bayer_info(fourcc: FourCc) -> Option<BayerInfo> {
    let code = fourcc.to_u32().to_le_bytes();
    let info = match &code {
        b"BA81" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"BA10" => BayerInfo {
            // V4L2_PIX_FMT_SGRBG10 (BA10): GRGR.. BGBG..
            pattern: BayerPattern::GRBG,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"BA12" => BayerInfo {
            // V4L2_PIX_FMT_SGRBG12 (BA12): GRGR.. BGBG..
            pattern: BayerPattern::GRBG,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"BA14" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"BG10" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"BG12" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"BG14" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"BG16" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"GB10" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"GB12" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"GB14" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"GB16" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"RG10" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"RG12" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"RG14" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"RG16" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"GR10" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"GR12" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"GR14" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"GR16" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"BYR2" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"RGGB" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"GRBG" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"GBRG" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"BGGR" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        // V4L2_PIX_FMT_S*10P / V4L2_PIX_FMT_S*12P (MIPI packed RAW10/RAW12).
        // bytes_per_sample = 0 signals packed bitstream.
        b"pBAA" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pGAA" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pgAA" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pRAA" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pBCC" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        b"pGCC" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        b"pgCC" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        b"pRCC" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        _ => return None,
    };
    Some(info)
}

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

fn sample_to_u8(data: &[u8], offset: usize, bps: usize, bit_depth: u8) -> u8 {
    if bps == 1 {
        data[offset]
    } else {
        let lo = data[offset];
        let hi = data[offset + 1];
        let v = u16::from_le_bytes([lo, hi]);
        let shift = (bit_depth.saturating_sub(8)) as u32;
        (v >> shift) as u8
    }
}

fn min_stride(width: usize, bit_depth: u8, bps: usize) -> usize {
    if bps > 0 {
        width.saturating_mul(bps)
    } else {
        match bit_depth {
            10 => width.div_ceil(4) * 5,
            12 => width.div_ceil(2) * 3,
            _ => 0,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_at(
    data: &[u8],
    stride: usize,
    bps: usize,
    bit_depth: u8,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> u8 {
    let xs = x.min(width.saturating_sub(1));
    let ys = y.min(height.saturating_sub(1));
    let row_off = ys.saturating_mul(stride);

    if bps > 0 {
        let offset = row_off.saturating_add(xs.saturating_mul(bps));
        return sample_to_u8(data, offset, bps, bit_depth);
    }

    // Packed MIPI RAW10/RAW12.
    let v = match bit_depth {
        10 => {
            // 4 pixels -> 5 bytes: [b0 b1 b2 b3 b4], where b4 carries 2 MSBs per pixel.
            let group = xs / 4;
            let idx = xs % 4;
            let base = row_off.saturating_add(group.saturating_mul(5));
            let b = data.get(base + idx).copied().unwrap_or(0) as u16;
            let b4 = data.get(base + 4).copied().unwrap_or(0) as u16;
            let msb = (b4 >> (idx * 2)) & 0x3;
            b | (msb << 8)
        }
        12 => {
            // 2 pixels -> 3 bytes: [b0 b1 b2], where b2 carries 4 MSBs per pixel.
            let pair = xs / 2;
            let idx = xs % 2;
            let base = row_off.saturating_add(pair.saturating_mul(3));
            let b0 = data.get(base).copied().unwrap_or(0) as u16;
            let b1 = data.get(base + 1).copied().unwrap_or(0) as u16;
            let b2 = data.get(base + 2).copied().unwrap_or(0) as u16;
            if idx == 0 {
                b0 | ((b2 & 0x0f) << 8)
            } else {
                b1 | (((b2 >> 4) & 0x0f) << 8)
            }
        }
        _ => 0,
    };
    let shift = (bit_depth.saturating_sub(8)) as u32;
    (v >> shift) as u8
}

/// Naive bilinear demosaic to RG24.
pub struct BayerToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    packed_pool: BufferPool,
    info: BayerInfo,
}

impl BayerToRgbDecoder {
    pub fn new(input: FourCc, info: BayerInfo, max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        let packed_bytes = max_width as usize * max_height as usize * 2;
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output: FourCc::new(*b"RG24"),
                name: "bayer2rgb",
                impl_name: "bayer-bilinear",
            },
            pool: BufferPool::with_limits(2, bytes, 4),
            packed_pool: BufferPool::with_limits(2, packed_bytes, 4),
            info,
        }
    }

    /// Decode into a caller-provided tightly-packed RGB24 buffer.
    ///
    /// `dst` must be at least `width * height * 3` bytes.
    pub fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("bayer frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        if width < 2 || height < 2 {
            return Err(CodecError::Codec("bayer frame too small".into()));
        }

        let stride = plane.stride().max(min_stride(
            width,
            self.info.bit_depth,
            self.info.bytes_per_sample,
        ));
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bayer stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("bayer plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("bayer output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bayer output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("bayer dst buffer too short".into()));
        }

        let dst = &mut dst[..out_len];
        let data = plane.data();
        if self.info.bytes_per_sample == 0 {
            let mut packed = self.packed_pool.lease();
            let packed_len = width
                .checked_mul(height)
                .and_then(|px| px.checked_mul(2))
                .ok_or_else(|| CodecError::Codec("bayer packed buffer overflow".into()))?;
            unsafe { packed.resize_uninit(packed_len) };
            let packed_u16 = unsafe {
                std::slice::from_raw_parts_mut(
                    packed.as_mut_slice().as_mut_ptr() as *mut u16,
                    width * height,
                )
            };
            unpack_mipi_packed_to_u16_le(
                packed_u16,
                data,
                stride,
                width,
                height,
                self.info.bit_depth,
            );
            demosaic_bilinear_u16_le(
                dst,
                packed.as_slice(),
                width,
                width,
                height,
                self.info.pattern,
                self.info.bit_depth,
            );
        } else {
            demosaic_bilinear_to_rg24(
                dst,
                data,
                stride,
                width,
                height,
                self.info.pattern,
                self.info.bit_depth,
                self.info.bytes_per_sample,
            );
        }

        Ok(FrameMeta::new(
            MediaFormat::new(
                self.descriptor.output,
                meta.format.resolution,
                meta.format.color,
            ),
            meta.timestamp,
        ))
    }
}

impl Codec for BayerToRgbDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let layout = plane_layout_from_dims(
            input.meta().format.resolution.width,
            input.meta().format.resolution.height,
            3,
        );
        let mut buf = self.pool.lease();
        unsafe { buf.resize_uninit(layout.len) };
        let meta = self.decode_into(&input, buf.as_mut_slice())?;

        Ok(unsafe {
            FrameLease::single_plane_uninit(
                meta,
                buf,
                layout.len,
                layout.stride,
            )
        })
    }
}

fn unpack_mipi_packed_to_u16_le(
    dst: &mut [u16],
    data: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    bit_depth: u8,
) {
    debug_assert!(dst.len() >= width.saturating_mul(height));
    dst.par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, dst_row)| {
            let src_row = &data[y * stride..][..stride];
            match bit_depth {
                10 => unpack_raw10_row(dst_row, src_row, width),
                12 => unpack_raw12_row(dst_row, src_row, width),
                _ => {
                    for (x, dst_px) in dst_row.iter_mut().enumerate().take(width) {
                        let v =
                            sample_at(data, stride, 0, bit_depth, x, y, width, height) as u16;
                        *dst_px = v.to_le();
                    }
                }
            }
        });
}

#[inline(always)]
fn unpack_raw10_row(dst: &mut [u16], src: &[u8], width: usize) {
    let mut x = 0usize;
    let mut off = 0usize;
    while x + 4 <= width {
        // 4 pixels -> 5 bytes: [b0 b1 b2 b3 b4], where b4 carries 2 MSBs per pixel.
        let b0 = unsafe { *src.get_unchecked(off) } as u16;
        let b1 = unsafe { *src.get_unchecked(off + 1) } as u16;
        let b2 = unsafe { *src.get_unchecked(off + 2) } as u16;
        let b3 = unsafe { *src.get_unchecked(off + 3) } as u16;
        let b4 = unsafe { *src.get_unchecked(off + 4) } as u16;
        unsafe {
            *dst.get_unchecked_mut(x) = (b0 | ((b4 & 0x03) << 8)).to_le();
            *dst.get_unchecked_mut(x + 1) = (b1 | (((b4 >> 2) & 0x03) << 8)).to_le();
            *dst.get_unchecked_mut(x + 2) = (b2 | (((b4 >> 4) & 0x03) << 8)).to_le();
            *dst.get_unchecked_mut(x + 3) = (b3 | (((b4 >> 6) & 0x03) << 8)).to_le();
        }
        x += 4;
        off += 5;
    }
    if x < width {
        // Tail: fall back to the generic sampler for the last 1-3 pixels.
        for (xs, dst_px) in dst.iter_mut().enumerate().take(width).skip(x) {
            let group = xs / 4;
            let idx = xs % 4;
            let base = group.saturating_mul(5);
            let b = src.get(base + idx).copied().unwrap_or(0) as u16;
            let b4 = src.get(base + 4).copied().unwrap_or(0) as u16;
            let msb = (b4 >> (idx * 2)) & 0x3;
            *dst_px = (b | (msb << 8)).to_le();
        }
    }
}

#[inline(always)]
fn unpack_raw12_row(dst: &mut [u16], src: &[u8], width: usize) {
    let mut x = 0usize;
    let mut off = 0usize;
    while x + 2 <= width {
        // 2 pixels -> 3 bytes: [b0 b1 b2], where b2 carries 4 MSBs per pixel.
        let b0 = unsafe { *src.get_unchecked(off) } as u16;
        let b1 = unsafe { *src.get_unchecked(off + 1) } as u16;
        let b2 = unsafe { *src.get_unchecked(off + 2) } as u16;
        unsafe {
            *dst.get_unchecked_mut(x) = (b0 | ((b2 & 0x0f) << 8)).to_le();
            *dst.get_unchecked_mut(x + 1) = (b1 | (((b2 >> 4) & 0x0f) << 8)).to_le();
        }
        x += 2;
        off += 3;
    }
    if x < width {
        // Tail: fall back to the generic sampler for the last pixel.
        let pair = x / 2;
        let idx = x % 2;
        let base = pair.saturating_mul(3);
        let b0 = src.get(base).copied().unwrap_or(0) as u16;
        let b1 = src.get(base + 1).copied().unwrap_or(0) as u16;
        let b2 = src.get(base + 2).copied().unwrap_or(0) as u16;
        dst[x] = (if idx == 0 {
            b0 | ((b2 & 0x0f) << 8)
        } else {
            b1 | (((b2 >> 4) & 0x0f) << 8)
        })
        .to_le();
    }
}

#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn demosaic_bilinear_to_rg24(
    dst: &mut [u8],
    data: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    pattern: BayerPattern,
    bit_depth: u8,
    bytes_per_sample: usize,
) {
    if bytes_per_sample == 2 && stride % 2 == 0 {
        demosaic_bilinear_u16_le(dst, data, stride / 2, width, height, pattern, bit_depth);
        return;
    }

    // Slow fallback: use the generic sampler (supports packed RAW10/RAW12).
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
fn demosaic_bilinear_u16_le(
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
    unsafe fn avg2_u16(a: std::arch::aarch64::uint16x8_t, b: std::arch::aarch64::uint16x8_t) -> std::arch::aarch64::uint16x8_t {
        use std::arch::aarch64::{
            vaddq_u32, vcombine_u16, vget_high_u16, vget_low_u16, vmovn_u32, vmovl_u16,
            vshrq_n_u32,
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
            vaddq_u32, vcombine_u16, vget_high_u16, vget_low_u16, vmovn_u32, vmovl_u16,
            vshrq_n_u32,
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
    unsafe fn shift_u16x8_to_u8(v: std::arch::aarch64::uint16x8_t, shift: u32) -> std::arch::aarch64::uint8x8_t {
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
                // Defensive: shifts > 8 shouldn't happen for 16-bit inputs.
                _ => vshrn_n_u16(v, 8),
            }
        }
    }

    // Borders: grayscale (center value replicated).
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
                            uint16x8_t, uint8x8x3_t, vbslq_u16, vld1q_u16, vmvnq_u16, vst3_u8,
                        };

                        // For the vector loop we always start at x=1; x parity does not change as we step by 8.
                        const MASK_START_EVEN: [u16; 8] = [0xFFFF, 0x0000, 0xFFFF, 0x0000, 0xFFFF, 0x0000, 0xFFFF, 0x0000];
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
                                        // even x: R, odd x: G (on red row)
                                        (c, g_lrud, diag, lr2, c, ud2)
                                    } else {
                                        // even x: G (on blue row), odd x: B
                                        (ud2, c, lr2, diag, g_lrud, c)
                                    }
                                }
                                BayerPattern::BGGR => {
                                    if y_is_even {
                                        // even x: B, odd x: G (on blue row)
                                        (diag, g_lrud, c, ud2, c, lr2)
                                    } else {
                                        // even x: G (on red row), odd x: R
                                        (lr2, c, ud2, c, g_lrud, diag)
                                    }
                                }
                                BayerPattern::GBRG => {
                                    if y_is_even {
                                        // even x: G (on blue row), odd x: B
                                        (ud2, c, lr2, diag, g_lrud, c)
                                    } else {
                                        // even x: R, odd x: G (on red row)
                                        (c, g_lrud, diag, lr2, c, ud2)
                                    }
                                }
                                BayerPattern::GRBG => {
                                    if y_is_even {
                                        // even x: G (on red row), odd x: R
                                        (lr2, c, ud2, c, g_lrud, diag)
                                    } else {
                                        // even x: B, odd x: G (on blue row)
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

                        // Tail.
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

#[cfg(feature = "image")]
impl ImageDecode for BayerToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

pub fn bayer_decoder_for(
    fourcc: FourCc,
    info: BayerInfo,
    max_width: u32,
    max_height: u32,
) -> Arc<dyn Codec> {
    Arc::new(BayerToRgbDecoder::new(fourcc, info, max_width, max_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_raw10_4px(p0: u16, p1: u16, p2: u16, p3: u16) -> [u8; 5] {
        let b0 = (p0 & 0xff) as u8;
        let b1 = (p1 & 0xff) as u8;
        let b2 = (p2 & 0xff) as u8;
        let b3 = (p3 & 0xff) as u8;
        let b4 = ((p0 >> 8) as u8 & 0x03)
            | (((p1 >> 8) as u8 & 0x03) << 2)
            | (((p2 >> 8) as u8 & 0x03) << 4)
            | (((p3 >> 8) as u8 & 0x03) << 6);
        [b0, b1, b2, b3, b4]
    }

    #[test]
    fn raw10_packed_sampling_matches_values() {
        let row = pack_raw10_4px(0x000, 0x155, 0x2aa, 0x3ff);
        let stride = row.len();
        let data = row.as_slice();
        let w = 4;
        let h = 1;
        assert_eq!(sample_at(data, stride, 0, 10, 0, 0, w, h), 0x00);
        assert_eq!(sample_at(data, stride, 0, 10, 1, 0, w, h), 0x55);
        assert_eq!(sample_at(data, stride, 0, 10, 2, 0, w, h), 0xaa);
        assert_eq!(sample_at(data, stride, 0, 10, 3, 0, w, h), 0xff);
    }

    #[test]
    fn raw10_unpack_matches_values() {
        let row = pack_raw10_4px(0x000, 0x155, 0x2aa, 0x3ff);
        let mut out = [0u16; 4];
        unpack_raw10_row(&mut out, &row, 4);
        assert_eq!(u16::from_le(out[0]), 0x000);
        assert_eq!(u16::from_le(out[1]), 0x155);
        assert_eq!(u16::from_le(out[2]), 0x2aa);
        assert_eq!(u16::from_le(out[3]), 0x3ff);
    }

    #[test]
    fn packed_raw10_decode_matches_unpacked() {
        let w = 4usize;
        let h = 4usize;
        let res = Resolution::new(w as u32, h as u32).unwrap();

        let mut raw = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                raw[y * w + x] = (((y * w + x) * 77) & 0x3ff) as u16;
            }
        }

        // Packed RAW10: 4 pixels -> 5 bytes, no padding for w=4.
        let packed_stride = 5usize;
        let mut packed = Vec::with_capacity(packed_stride * h);
        for y in 0..h {
            let row = &raw[y * w..(y + 1) * w];
            packed.extend_from_slice(&pack_raw10_4px(row[0], row[1], row[2], row[3]));
        }

        // Unpacked u16 little-endian: w * 2 bytes per row.
        let unpacked_stride = w * 2;
        let mut unpacked = vec![0u8; unpacked_stride * h];
        for y in 0..h {
            for x in 0..w {
                let v = raw[y * w + x].to_le_bytes();
                let o = y * unpacked_stride + x * 2;
                unpacked[o] = v[0];
                unpacked[o + 1] = v[1];
            }
        }

        let packed_fourcc = FourCc::new(*b"pRAA");
        let unpacked_fourcc = FourCc::new(*b"RG10");
        let packed_info = bayer_info(packed_fourcc).unwrap();
        let unpacked_info = bayer_info(unpacked_fourcc).unwrap();
        let packed_dec = BayerToRgbDecoder::new(packed_fourcc, packed_info, res.width.get(), res.height.get());
        let unpacked_dec =
            BayerToRgbDecoder::new(unpacked_fourcc, unpacked_info, res.width.get(), res.height.get());

        let pool = BufferPool::with_limits(2, packed.len().max(unpacked.len()), 4);

        let mut packed_buf = pool.lease();
        packed_buf.resize(packed.len());
        packed_buf.as_mut_slice().copy_from_slice(&packed);
        let packed_frame = FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(packed_fourcc, res, ColorSpace::Unknown), 0),
            packed_buf,
            packed.len(),
            packed_stride,
        );

        let mut unpacked_buf = pool.lease();
        unpacked_buf.resize(unpacked.len());
        unpacked_buf.as_mut_slice().copy_from_slice(&unpacked);
        let unpacked_frame = FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(unpacked_fourcc, res, ColorSpace::Unknown),
                0,
            ),
            unpacked_buf,
            unpacked.len(),
            unpacked_stride,
        );

        let a = packed_dec.process(packed_frame).unwrap();
        let b = unpacked_dec.process(unpacked_frame).unwrap();
        let a_plane = a.planes();
        let b_plane = b.planes();
        assert_eq!(a_plane.len(), 1);
        assert_eq!(b_plane.len(), 1);
        assert_eq!(a_plane[0].data(), b_plane[0].data());
    }
}
