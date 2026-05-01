use rayon::prelude::*;

pub(super) fn min_stride(width: usize, bit_depth: u8, bps: usize) -> usize {
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

// Sampling needs raw layout fields in the inner path; grouping them would obscure call sites.
#[allow(clippy::too_many_arguments)]
pub(super) fn sample_at(
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

    let v = match bit_depth {
        10 => {
            let group = xs / 4;
            let idx = xs % 4;
            let base = row_off.saturating_add(group.saturating_mul(5));
            let b = data.get(base + idx).copied().unwrap_or(0) as u16;
            let b4 = data.get(base + 4).copied().unwrap_or(0) as u16;
            let msb = (b4 >> (idx * 2)) & 0x3;
            b | (msb << 8)
        }
        12 => {
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

pub(super) fn unpack_mipi_packed_to_u16_le(
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
                        let v = sample_at(data, stride, 0, bit_depth, x, y, width, height) as u16;
                        *dst_px = v.to_le();
                    }
                }
            }
        });
}

#[inline(always)]
pub(super) fn unpack_raw10_row(dst: &mut [u16], src: &[u8], width: usize) {
    let mut x = 0usize;
    let mut off = 0usize;
    while x + 4 <= width {
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
