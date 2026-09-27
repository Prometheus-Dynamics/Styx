//! SSE2 8x8 tile transposes (see `scalar::transpose_tile`): output row `k` is input column
//! `k`, reversed when `reverse`. SSE2 for 1-, 2- and 4-byte pixels, SSSE3 for 3-byte pixels.

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[inline(always)]
unsafe fn load(p: *const u8) -> __m128i {
    // SAFETY: the caller guarantees 16 readable bytes.
    unsafe { _mm_loadu_si128(p.cast()) }
}

#[inline(always)]
unsafe fn store(p: *mut u8, v: __m128i) {
    // SAFETY: the caller guarantees 16 writable bytes.
    unsafe { _mm_storeu_si128(p.cast(), v) }
}

/// Transpose 8 rows of 8 bytes (in the low halves of `r`): the result holds columns (0, 1),
/// (2, 3), (4, 5), (6, 7), each column 8 bytes.
#[inline(always)]
unsafe fn transpose_u8x8(r: [__m128i; 8]) -> [__m128i; 4] {
    // SAFETY: plain SSE2 unpacks, enabled by the caller.
    unsafe {
        let a0 = _mm_unpacklo_epi8(r[0], r[1]);
        let a1 = _mm_unpacklo_epi8(r[2], r[3]);
        let a2 = _mm_unpacklo_epi8(r[4], r[5]);
        let a3 = _mm_unpacklo_epi8(r[6], r[7]);
        let b0 = _mm_unpacklo_epi16(a0, a1);
        let b1 = _mm_unpackhi_epi16(a0, a1);
        let b2 = _mm_unpacklo_epi16(a2, a3);
        let b3 = _mm_unpackhi_epi16(a2, a3);
        [
            _mm_unpacklo_epi32(b0, b2),
            _mm_unpackhi_epi32(b0, b2),
            _mm_unpacklo_epi32(b1, b3),
            _mm_unpackhi_epi32(b1, b3),
        ]
    }
}

/// # Safety
/// SSE2 must be available; `src` readable for 8 rows of 8 bytes at `src_stride`, `dst`
/// writable for 8 rows of 8 bytes at `dst_stride`.
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn transpose_tile_u8_sse2(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: 8 rows of 8 bytes in and out, per the caller.
    unsafe {
        let r: [__m128i; 8] =
            std::array::from_fn(|i| _mm_loadl_epi64(src.add(i * src_stride).cast()));
        let pairs = transpose_u8x8(r);
        for (p, pair) in pairs.into_iter().enumerate() {
            let mut cols = [0u64; 2];
            _mm_storeu_si128(cols.as_mut_ptr().cast(), pair);
            for (h, col) in cols.into_iter().enumerate() {
                let col = if reverse { col.swap_bytes() } else { col };
                let out = dst.offset((2 * p + h) as isize * dst_stride);
                out.cast::<u64>().write_unaligned(col);
            }
        }
    }
}

/// Reverse the 8 16-bit lanes of `v`.
#[inline(always)]
unsafe fn rev_u16x8(v: __m128i) -> __m128i {
    // SAFETY: plain SSE2 shuffles, enabled by the caller.
    unsafe {
        _mm_shuffle_epi32::<0x4E>(_mm_shufflehi_epi16::<0x1B>(_mm_shufflelo_epi16::<0x1B>(v)))
    }
}

/// # Safety
/// As [`transpose_tile_u8_sse2`], with 2-byte pixels (8 rows of 16 bytes).
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn transpose_tile_u16_sse2(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: as above.
    unsafe {
        let r: [__m128i; 8] = std::array::from_fn(|i| load(src.add(i * src_stride)));
        // a[2j]: pixels 0-3 of rows 2j, 2j+1 interleaved; a[2j+1]: pixels 4-7.
        let a: [__m128i; 8] = std::array::from_fn(|i| {
            let (x, y) = (r[i / 2 * 2], r[i / 2 * 2 + 1]);
            if i % 2 == 0 {
                _mm_unpacklo_epi16(x, y)
            } else {
                _mm_unpackhi_epi16(x, y)
            }
        });
        // Rows 0-3 of columns (0,1), (2,3), (4,5), (6,7), then rows 4-7 of the same.
        let b = [
            _mm_unpacklo_epi32(a[0], a[2]),
            _mm_unpackhi_epi32(a[0], a[2]),
            _mm_unpacklo_epi32(a[1], a[3]),
            _mm_unpackhi_epi32(a[1], a[3]),
            _mm_unpacklo_epi32(a[4], a[6]),
            _mm_unpackhi_epi32(a[4], a[6]),
            _mm_unpacklo_epi32(a[5], a[7]),
            _mm_unpackhi_epi32(a[5], a[7]),
        ];
        for k in 0..8 {
            let (top, bottom) = (b[k / 2], b[k / 2 + 4]);
            let col = if k % 2 == 0 {
                _mm_unpacklo_epi64(top, bottom)
            } else {
                _mm_unpackhi_epi64(top, bottom)
            };
            let col = if reverse { rev_u16x8(col) } else { col };
            store(dst.offset(k as isize * dst_stride), col);
        }
    }
}

/// # Safety
/// As [`transpose_tile_u8_sse2`], with 4-byte pixels (8 rows of 32 bytes).
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn transpose_tile_u32_sse2(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    // SAFETY: as above. rows[i][h] holds pixels 4h..4h+4 of row i.
    unsafe {
        let rows: [[__m128i; 2]; 8] = std::array::from_fn(|i| {
            let p = src.add(i * src_stride);
            [load(p), load(p.add(16))]
        });
        // Columns of the 4x4 block with rows `a..d`.
        let block = |a: __m128i, b: __m128i, c: __m128i, d: __m128i| {
            let (t0, t1) = (_mm_unpacklo_epi32(a, b), _mm_unpacklo_epi32(c, d));
            let (t2, t3) = (_mm_unpackhi_epi32(a, b), _mm_unpackhi_epi32(c, d));
            [
                _mm_unpacklo_epi64(t0, t1),
                _mm_unpackhi_epi64(t0, t1),
                _mm_unpacklo_epi64(t2, t3),
                _mm_unpackhi_epi64(t2, t3),
            ]
        };
        #[allow(clippy::needless_range_loop)] // `h` indexes every row of the tile
        for h in 0..2 {
            let top = block(rows[0][h], rows[1][h], rows[2][h], rows[3][h]);
            let bottom = block(rows[4][h], rows[5][h], rows[6][h], rows[7][h]);
            for c in 0..4 {
                let out = dst.offset((4 * h + c) as isize * dst_stride);
                let (first, second) = if reverse {
                    (
                        _mm_shuffle_epi32::<0x1B>(bottom[c]),
                        _mm_shuffle_epi32::<0x1B>(top[c]),
                    )
                } else {
                    (top[c], bottom[c])
                };
                store(out, first);
                store(out.add(16), second);
            }
        }
    }
}

/// Byte masks: channel `c` of 8 RGB pixels from the row's first 16 bytes (`.0`) and its bytes
/// 16..24 loaded as the low half of a second vector (`.1`), into the low 8 lanes.
const fn plane_masks(c: usize) -> ([u8; 16], [u8; 16]) {
    let (mut a, mut b) = ([0x80u8; 16], [0x80u8; 16]);
    let mut px = 0;
    while px < 8 {
        let byte = 3 * px + c;
        if byte < 16 {
            a[px] = byte as u8;
        } else {
            b[px] = (byte - 16) as u8;
        }
        px += 1;
    }
    (a, b)
}

/// Masks interleaving planes back: output bytes `16 * half..` of 8 RGB pixels from `rg` (R in
/// the low 8 lanes, G in the high 8) and `b` (B in the low 8).
const fn interleave_masks(half: usize) -> ([u8; 16], [u8; 16]) {
    let (mut from_rg, mut from_b) = ([0x80u8; 16], [0x80u8; 16]);
    let mut j = 0;
    while j < 16 {
        let out = 16 * half + j;
        if out < 24 {
            let (px, c) = (out / 3, out % 3);
            match c {
                0 => from_rg[j] = px as u8,
                1 => from_rg[j] = (8 + px) as u8,
                _ => from_b[j] = px as u8,
            }
        }
        j += 1;
    }
    (from_rg, from_b)
}

#[inline(always)]
unsafe fn shuffle(v: __m128i, m: [u8; 16]) -> __m128i {
    // SAFETY: SSSE3 is enabled by the caller; the mask is a local array.
    unsafe { _mm_shuffle_epi8(v, _mm_loadu_si128(m.as_ptr().cast())) }
}

/// # Safety
/// SSSE3 must be available; as [`transpose_tile_u8_sse2`], with 3-byte pixels (8 rows of 24
/// bytes).
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn transpose_tile_rgb_ssse3(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    const PLANES: [([u8; 16], [u8; 16]); 3] = [plane_masks(0), plane_masks(1), plane_masks(2)];
    const OUT: [([u8; 16], [u8; 16]); 2] = [interleave_masks(0), interleave_masks(1)];
    // Reverse the 8 bytes of the low half.
    const REV8: [u8; 16] = [
        7, 6, 5, 4, 3, 2, 1, 0, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
    ];
    // SAFETY: 8 rows of 24 bytes in and out, per the caller.
    unsafe {
        // Each row split into its R, G and B planes, then each plane transposed.
        let mut planes = [[_mm_setzero_si128(); 8]; 3];
        for i in 0..8 {
            let p = src.add(i * src_stride);
            let (a, b) = (load(p), _mm_loadl_epi64(p.add(16).cast()));
            for (plane, (ma, mb)) in planes.iter_mut().zip(PLANES) {
                plane[i] = _mm_or_si128(shuffle(a, ma), shuffle(b, mb));
            }
        }
        let cols = planes.map(|plane| transpose_u8x8(plane));
        for k in 0..8 {
            // Column k of each plane: 8 bytes, the low or high half of pair k / 2.
            let col = |c: usize| {
                let pair = cols[c][k / 2];
                let v = if k % 2 == 0 {
                    pair
                } else {
                    _mm_srli_si128::<8>(pair)
                };
                if reverse { shuffle(v, REV8) } else { v }
            };
            let rg = _mm_unpacklo_epi64(col(0), col(1));
            let b = col(2);
            let px = |(m_rg, m_b): ([u8; 16], [u8; 16])| {
                _mm_or_si128(shuffle(rg, m_rg), shuffle(b, m_b))
            };
            let out = dst.offset(k as isize * dst_stride);
            store(out, px(OUT[0]));
            _mm_storel_epi64(out.add(16).cast(), px(OUT[1]));
        }
    }
}
