//! NEON 8x8 tile transposes (see `scalar::transpose_tile`): output row `k` is input column `k`,
//! reversed when `reverse`.

use std::arch::aarch64::*;

/// Columns of an 8x8 byte tile given its rows.
#[inline(always)]
unsafe fn transpose_u8x8(r: [uint8x8_t; 8]) -> [uint8x8_t; 8] {
    unsafe {
        let t0 = vtrn_u8(r[0], r[1]);
        let t1 = vtrn_u8(r[2], r[3]);
        let t2 = vtrn_u8(r[4], r[5]);
        let t3 = vtrn_u8(r[6], r[7]);
        let u0 = vtrn_u16(vreinterpret_u16_u8(t0.0), vreinterpret_u16_u8(t1.0));
        let u1 = vtrn_u16(vreinterpret_u16_u8(t0.1), vreinterpret_u16_u8(t1.1));
        let u2 = vtrn_u16(vreinterpret_u16_u8(t2.0), vreinterpret_u16_u8(t3.0));
        let u3 = vtrn_u16(vreinterpret_u16_u8(t2.1), vreinterpret_u16_u8(t3.1));
        let v0 = vtrn_u32(vreinterpret_u32_u16(u0.0), vreinterpret_u32_u16(u2.0));
        let v1 = vtrn_u32(vreinterpret_u32_u16(u1.0), vreinterpret_u32_u16(u3.0));
        let v2 = vtrn_u32(vreinterpret_u32_u16(u0.1), vreinterpret_u32_u16(u2.1));
        let v3 = vtrn_u32(vreinterpret_u32_u16(u1.1), vreinterpret_u32_u16(u3.1));
        [v0.0, v1.0, v2.0, v3.0, v0.1, v1.1, v2.1, v3.1].map(|v| vreinterpret_u8_u32(v))
    }
}

/// # Safety
/// NEON must be available; `src` readable for 8 rows of 8 bytes at `src_stride`, `dst`
/// writable for 8 rows of 8 bytes at `dst_stride`.
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn transpose_tile_u8(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: 8 rows of 8 bytes in and out, per the caller.
    unsafe {
        let rows = std::array::from_fn(|i| vld1_u8(src.add(i * src_stride)));
        for (k, col) in transpose_u8x8(rows).into_iter().enumerate() {
            let col = if reverse { vrev64_u8(col) } else { col };
            vst1_u8(dst.offset(k as isize * dst_stride), col);
        }
    }
}

/// # Safety
/// As [`transpose_tile_u8`], with 3-byte pixels (8 rows of 24 bytes).
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn transpose_tile_rgb(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: as above; each channel plane is transposed on its own.
    unsafe {
        let rows: [uint8x8x3_t; 8] = std::array::from_fn(|i| vld3_u8(src.add(i * src_stride)));
        let planes = [
            transpose_u8x8(rows.map(|r| r.0)),
            transpose_u8x8(rows.map(|r| r.1)),
            transpose_u8x8(rows.map(|r| r.2)),
        ];
        #[allow(clippy::needless_range_loop)] // `k` indexes all three planes
        for k in 0..8 {
            let mut px = uint8x8x3_t(planes[0][k], planes[1][k], planes[2][k]);
            if reverse {
                px = uint8x8x3_t(vrev64_u8(px.0), vrev64_u8(px.1), vrev64_u8(px.2));
            }
            vst3_u8(dst.offset(k as isize * dst_stride), px);
        }
    }
}

/// # Safety
/// As [`transpose_tile_u8`], with 2-byte pixels (8 rows of 16 bytes).
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn transpose_tile_u16(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: as above.
    unsafe {
        let r: [uint16x8_t; 8] =
            std::array::from_fn(|i| vreinterpretq_u16_u8(vld1q_u8(src.add(i * src_stride))));
        let t = [
            vtrnq_u16(r[0], r[1]),
            vtrnq_u16(r[2], r[3]),
            vtrnq_u16(r[4], r[5]),
            vtrnq_u16(r[6], r[7]),
        ];
        let w = |v: uint16x8_t| vreinterpretq_u32_u16(v);
        // u0/u2: columns 0 and 4 (.0), 2 and 6 (.1); u1/u3: columns 1 and 5, 3 and 7; rows 0-3
        // in u0/u1, rows 4-7 in u2/u3.
        let u0 = vtrnq_u32(w(t[0].0), w(t[1].0));
        let u1 = vtrnq_u32(w(t[0].1), w(t[1].1));
        let u2 = vtrnq_u32(w(t[2].0), w(t[3].0));
        let u3 = vtrnq_u32(w(t[2].1), w(t[3].1));
        let lo = |a: uint32x4_t, b: uint32x4_t| vcombine_u32(vget_low_u32(a), vget_low_u32(b));
        let hi = |a: uint32x4_t, b: uint32x4_t| vcombine_u32(vget_high_u32(a), vget_high_u32(b));
        let cols = [
            lo(u0.0, u2.0),
            lo(u1.0, u3.0),
            lo(u0.1, u2.1),
            lo(u1.1, u3.1),
            hi(u0.0, u2.0),
            hi(u1.0, u3.0),
            hi(u0.1, u2.1),
            hi(u1.1, u3.1),
        ];
        for (k, col) in cols.into_iter().enumerate() {
            let mut col = vreinterpretq_u16_u32(col);
            if reverse {
                let r = vrev64q_u16(col);
                col = vextq_u16::<4>(r, r);
            }
            vst1q_u8(
                dst.offset(k as isize * dst_stride),
                vreinterpretq_u8_u16(col),
            );
        }
    }
}

/// # Safety
/// As [`transpose_tile_u8`], with 4-byte pixels (8 rows of 32 bytes).
#[target_feature(enable = "neon")]
pub(in crate::simd) unsafe fn transpose_tile_u32(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: as above. The tile is four 4x4 blocks: rows[i][h] holds pixels 4h..4h+4 of row i.
    unsafe {
        let rows: [[uint32x4_t; 2]; 8] = std::array::from_fn(|i| {
            let p = src.add(i * src_stride);
            [
                vreinterpretq_u32_u8(vld1q_u8(p)),
                vreinterpretq_u32_u8(vld1q_u8(p.add(16))),
            ]
        });
        // Columns of the 4x4 block with rows `a..d`.
        let block = |a: uint32x4_t, b: uint32x4_t, c: uint32x4_t, d: uint32x4_t| {
            let t0 = vtrnq_u32(a, b);
            let t1 = vtrnq_u32(c, d);
            [
                vcombine_u32(vget_low_u32(t0.0), vget_low_u32(t1.0)),
                vcombine_u32(vget_low_u32(t0.1), vget_low_u32(t1.1)),
                vcombine_u32(vget_high_u32(t0.0), vget_high_u32(t1.0)),
                vcombine_u32(vget_high_u32(t0.1), vget_high_u32(t1.1)),
            ]
        };
        let rev = |v: uint32x4_t| {
            let r = vrev64q_u32(v);
            vextq_u32::<2>(r, r)
        };
        #[allow(clippy::needless_range_loop)] // `h` indexes every row of the tile
        for h in 0..2 {
            // Columns 4h..4h+4: their first four entries from rows 0-3, the rest from rows 4-7.
            let top = block(rows[0][h], rows[1][h], rows[2][h], rows[3][h]);
            let bottom = block(rows[4][h], rows[5][h], rows[6][h], rows[7][h]);
            for c in 0..4 {
                let out = dst.offset((4 * h + c) as isize * dst_stride);
                let (first, second) = if reverse {
                    (rev(bottom[c]), rev(top[c]))
                } else {
                    (top[c], bottom[c])
                };
                vst1q_u8(out, vreinterpretq_u8_u32(first));
                vst1q_u8(out.add(16), vreinterpretq_u8_u32(second));
            }
        }
    }
}
