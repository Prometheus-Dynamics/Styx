//! x86 row kernels. Each processes whole blocks from the start of the row (reverse: see
//! [`reverse_rgb_ssse3`]) and returns the pixels it wrote; slices must hold `width` pixels.
//! Some store a full vector whose last bytes belong to the next block: they stop early enough
//! that those bytes are inside the row and are rewritten by the next block or the scalar tail.

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// A shuffle mask from 16 byte indexes (0x80: zero).
#[inline(always)]
unsafe fn mask(m: [u8; 16]) -> __m128i {
    // SAFETY: an unaligned 16-byte load of a local array.
    unsafe { _mm_loadu_si128(m.as_ptr().cast()) }
}

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

const SWAP_RB_5PX: [u8; 16] = [2, 1, 0, 5, 4, 3, 8, 7, 6, 11, 10, 9, 14, 13, 12, 15];

/// # Safety
/// SSSE3 must be available; both rows hold `width` 3-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn swap_rb24_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let bytes = width * 3;
    let mut px = 0;
    // 5 pixels (15 bytes) per step; the 16th byte stored is rewritten by the next step.
    while px * 3 + 16 <= bytes {
        // SAFETY: 16 bytes at `px * 3` in both rows.
        unsafe {
            let v = _mm_shuffle_epi8(load(src.as_ptr().add(px * 3)), mask(SWAP_RB_5PX));
            store(dst.as_mut_ptr().add(px * 3), v);
        }
        px += 5;
    }
    px
}

/// # Safety
/// AVX2 must be available; both rows hold `width` 3-byte pixels.
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn swap_rb24_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let bytes = width * 3;
    let mut px = 0;
    // 10 pixels per step: two 15-byte halves in the two 128-bit lanes.
    while px * 3 + 31 <= bytes {
        // SAFETY: 31 bytes at `px * 3` in both rows (the upper half's load ends there).
        unsafe {
            let s = src.as_ptr().add(px * 3);
            let v = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(load(s)), load(s.add(15)));
            let m = _mm256_broadcastsi128_si256(mask(SWAP_RB_5PX));
            let v = _mm256_shuffle_epi8(v, m);
            let d = dst.as_mut_ptr().add(px * 3);
            // The lower half first: the upper half then rewrites its spilled 16th byte.
            store(d, _mm256_castsi256_si128(v));
            store(d.add(15), _mm256_extracti128_si256::<1>(v));
        }
        px += 10;
    }
    px
}

/// BGRA ↔ RGBA in 32-bit lanes with SSE2 masks and shifts.
#[inline(always)]
unsafe fn swap_rb32_lanes(v: __m128i) -> __m128i {
    // SAFETY: plain SSE2 arithmetic, enabled by the caller.
    unsafe {
        let keep = _mm_and_si128(v, _mm_set1_epi32(0xFF00_FF00u32 as i32));
        let low = _mm_and_si128(_mm_srli_epi32::<16>(v), _mm_set1_epi32(0xFF));
        let high = _mm_slli_epi32::<16>(_mm_and_si128(v, _mm_set1_epi32(0xFF)));
        _mm_or_si128(keep, _mm_or_si128(low, high))
    }
}

/// # Safety
/// SSE2 must be available; both rows hold `width` 4-byte pixels.
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn swap_rb32_sse2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 4;
    for i in 0..blocks {
        // SAFETY: 16 bytes of each row.
        unsafe {
            store(
                dst.as_mut_ptr().add(i * 16),
                swap_rb32_lanes(load(src.as_ptr().add(i * 16))),
            )
        };
    }
    blocks * 4
}

const SWAP_RB_4PX32: [u8; 16] = [2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15];

/// # Safety
/// SSSE3 must be available; both rows hold `width` 4-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn swap_rb32_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 4;
    for i in 0..blocks {
        // SAFETY: 16 bytes of each row.
        unsafe {
            let v = _mm_shuffle_epi8(load(src.as_ptr().add(i * 16)), mask(SWAP_RB_4PX32));
            store(dst.as_mut_ptr().add(i * 16), v);
        }
    }
    blocks * 4
}

/// # Safety
/// AVX2 must be available; both rows hold `width` 4-byte pixels.
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn swap_rb32_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 8;
    for i in 0..blocks {
        // SAFETY: 32 bytes of each row.
        unsafe {
            let v = _mm256_loadu_si256(src.as_ptr().add(i * 32).cast());
            let v = _mm256_shuffle_epi8(v, _mm256_broadcastsi128_si256(mask(SWAP_RB_4PX32)));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32).cast(), v);
        }
    }
    blocks * 8
}

/// # Safety
/// SSSE3 must be available; `src` holds `width` 4-byte pixels, `dst` `width` 3-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn x32_to_rgb24_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> usize {
    let m = if swap {
        [
            2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, 0x80, 0x80, 0x80, 0x80,
        ]
    } else {
        [
            0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 0x80, 0x80, 0x80, 0x80,
        ]
    };
    let mut px = 0;
    // 4 pixels per step; 12 bytes out, 4 spilled.
    while px + 4 <= width && px * 3 + 16 <= width * 3 {
        // SAFETY: 16 bytes at `px * 4` and at `px * 3`.
        unsafe {
            let v = _mm_shuffle_epi8(load(src.as_ptr().add(px * 4)), mask(m));
            store(dst.as_mut_ptr().add(px * 3), v);
        }
        px += 4;
    }
    px
}

/// # Safety
/// AVX2 must be available; as [`x32_to_rgb24_ssse3`].
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn x32_to_rgb24_avx2(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> usize {
    let m = if swap {
        [
            2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, 0x80, 0x80, 0x80, 0x80,
        ]
    } else {
        [
            0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 0x80, 0x80, 0x80, 0x80,
        ]
    };
    let mut px = 0;
    // 8 pixels per step: 24 bytes out, 8 spilled.
    while px + 8 <= width && px * 3 + 32 <= width * 3 {
        // SAFETY: 32 bytes at `px * 4` and at `px * 3`.
        unsafe {
            let v = _mm256_loadu_si256(src.as_ptr().add(px * 4).cast());
            let v = _mm256_shuffle_epi8(v, _mm256_broadcastsi128_si256(mask(m)));
            // Pack the 12 bytes of each lane together.
            let v = _mm256_permutevar8x32_epi32(v, _mm256_setr_epi32(0, 1, 2, 4, 5, 6, 7, 7));
            _mm256_storeu_si256(dst.as_mut_ptr().add(px * 3).cast(), v);
        }
        px += 8;
    }
    px
}

/// Shuffles gathering channel byte `offset` of 8 `bpp`-byte pixels into 16-bit lanes: pixels
/// whose byte lies in the first 16-byte load (at 0) come from `.0`, the rest from the second
/// load (at `8 * bpp - 16`) via `.1`.
const fn gather_masks(bpp: usize, offset: usize) -> ([u8; 16], [u8; 16]) {
    let (mut a, mut b) = ([0x80u8; 16], [0x80u8; 16]);
    let second = 8 * bpp - 16;
    let mut px = 0;
    while px < 8 {
        let byte = px * bpp + offset;
        if byte < 16 {
            a[2 * px] = byte as u8;
        } else {
            b[2 * px] = (byte - second) as u8;
        }
        px += 1;
    }
    (a, b)
}

/// # Safety
/// SSSE3 must be available; `src` holds `width` pixels of `layout`, `dst` `width` bytes.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn rgb_to_luma_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: super::super::ColorLayout,
) -> usize {
    let bpp = layout.bytes_per_pixel();
    let [ro, go, bo] = layout.rgb_offsets();
    let masks = |o: usize| match (bpp, o) {
        (3, 0) => const { gather_masks(3, 0) },
        (3, 1) => const { gather_masks(3, 1) },
        (3, _) => const { gather_masks(3, 2) },
        (_, 0) => const { gather_masks(4, 0) },
        (_, 1) => const { gather_masks(4, 1) },
        _ => const { gather_masks(4, 2) },
    };
    let blocks = width / 8;
    for i in 0..blocks {
        // SAFETY: 8 pixels (8 * bpp bytes: two 16-byte loads ending at the block's end) in,
        // 8 bytes out. Each channel lands in 16-bit lanes; 77 + 150 + 29 = 256, so the weighted
        // sum fits 16 bits.
        unsafe {
            let p = src.as_ptr().add(i * 8 * bpp);
            let (a, b) = (load(p), load(p.add(8 * bpp - 16)));
            let channel = |o: usize| {
                let (ma, mb) = masks(o);
                _mm_or_si128(_mm_shuffle_epi8(a, mask(ma)), _mm_shuffle_epi8(b, mask(mb)))
            };
            let sum = _mm_add_epi16(
                _mm_add_epi16(
                    _mm_mullo_epi16(channel(ro), _mm_set1_epi16(77)),
                    _mm_mullo_epi16(channel(go), _mm_set1_epi16(150)),
                ),
                _mm_mullo_epi16(channel(bo), _mm_set1_epi16(29)),
            );
            let y = _mm_packus_epi16(_mm_srli_epi16::<8>(sum), _mm_setzero_si128());
            _mm_storel_epi64(dst.as_mut_ptr().add(i * 8).cast(), y);
        }
    }
    blocks * 8
}

/// # Safety
/// SSSE3 must be available; `src` holds `width` 3-byte pixels, `dst` `width` 4-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn rgb24_to_rgba_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    const SPREAD: [u8; 16] = [0, 1, 2, 0x80, 3, 4, 5, 0x80, 6, 7, 8, 0x80, 9, 10, 11, 0x80];
    let mut px = 0;
    // 4 pixels per step: 12 bytes read of a 16-byte load, so stop 4 bytes before the row end.
    while px + 4 <= width && px * 3 + 16 <= width * 3 {
        // SAFETY: 16 bytes at `px * 3`, 16 bytes at `px * 4`.
        unsafe {
            let v = _mm_shuffle_epi8(load(src.as_ptr().add(px * 3)), mask(SPREAD));
            let v = _mm_or_si128(v, _mm_set1_epi32(0xFF00_0000u32 as i32));
            store(dst.as_mut_ptr().add(px * 4), v);
        }
        px += 4;
    }
    px
}

const GRAY_TO_RGB: [[u8; 16]; 3] = [
    [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5],
    [5, 5, 6, 6, 6, 7, 7, 7, 8, 8, 8, 9, 9, 9, 10, 10],
    [
        10, 11, 11, 11, 12, 12, 12, 13, 13, 13, 14, 14, 14, 15, 15, 15,
    ],
];

/// 16 grey bytes as 48 RGB bytes.
#[inline(always)]
unsafe fn store_gray_as_rgb(dst: *mut u8, g: __m128i) {
    // SAFETY: the caller guarantees 48 writable bytes; SSSE3 is enabled by the caller.
    unsafe {
        for (i, m) in GRAY_TO_RGB.iter().enumerate() {
            store(dst.add(16 * i), _mm_shuffle_epi8(g, mask(*m)));
        }
    }
}

/// # Safety
/// SSSE3 must be available; `src` holds `width` bytes, `dst` `width` 3-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn gray8_to_rgb24_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 16 bytes in, 48 out.
        unsafe { store_gray_as_rgb(dst.as_mut_ptr().add(i * 48), load(src.as_ptr().add(i * 16))) };
    }
    blocks * 16
}

/// # Safety
/// AVX2 must be available; as [`gray8_to_rgb24_ssse3`].
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn gray8_to_rgb24_avx2(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 16;
    // SAFETY: loads of local mask arrays.
    let (m01, m2) = unsafe {
        (
            _mm256_setr_m128i(mask(GRAY_TO_RGB[0]), mask(GRAY_TO_RGB[1])),
            mask(GRAY_TO_RGB[2]),
        )
    };
    for i in 0..blocks {
        // SAFETY: 16 bytes in, 48 out.
        unsafe {
            let g = load(src.as_ptr().add(i * 16));
            let d = dst.as_mut_ptr().add(i * 48);
            let lo = _mm256_shuffle_epi8(_mm256_broadcastsi128_si256(g), m01);
            _mm256_storeu_si256(d.cast(), lo);
            store(d.add(32), _mm_shuffle_epi8(g, m2));
        }
    }
    blocks * 16
}

/// # Safety
/// SSSE3 must be available; `src` holds `width` 2-byte pixels, `dst` `width` 3-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn gray16le_to_rgb24_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 32 bytes in, 48 out. The high bytes of each 16-bit value, packed.
        unsafe {
            let s = src.as_ptr().add(i * 32);
            let g = _mm_packus_epi16(
                _mm_srli_epi16::<8>(load(s)),
                _mm_srli_epi16::<8>(load(s.add(16))),
            );
            store_gray_as_rgb(dst.as_mut_ptr().add(i * 48), g);
        }
    }
    blocks * 16
}

/// Output bytes `16 * half..16 * half + 16` of 8 swapped RGB24 pixels, whose unswapped bytes
/// 0..16 are in `a` and 16..24 in `b`, as shuffles of `a` and `b`.
const fn swap_masks(half: usize) -> ([u8; 16], [u8; 16]) {
    let (mut ma, mut mb) = ([0x80u8; 16], [0x80u8; 16]);
    let mut j = 0;
    while j < 16 {
        let out = 16 * half + j;
        if out < 24 {
            let src = 3 * (out / 3) + (2 - out % 3);
            if src < 16 {
                ma[j] = src as u8;
            } else {
                mb[j] = (src - 16) as u8;
            }
        }
        j += 1;
    }
    (ma, mb)
}

/// # Safety
/// SSE2 must be available (plus SSSE3 when `swap`); `src` holds `width` 6-byte pixels, `dst`
/// `width` 3-byte pixels.
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn rgb48le_to_rgb24_sse2(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 8;
    for i in 0..blocks {
        // SAFETY: 48 bytes in, 24 out. The high bytes of all 16-bit values, in order, are the
        // RGB24 bytes.
        unsafe {
            let s = src.as_ptr().add(i * 48);
            let hi = |k: usize| _mm_srli_epi16::<8>(load(s.add(16 * k)));
            let a = _mm_packus_epi16(hi(0), hi(1));
            let b = _mm_packus_epi16(hi(2), _mm_setzero_si128());
            let d = dst.as_mut_ptr().add(i * 24);
            store(d, a);
            _mm_storel_epi64(d.add(16).cast(), b);
        }
    }
    blocks * 8
}

/// # Safety
/// SSSE3 must be available; as [`rgb48le_to_rgb24_sse2`], producing BGR-swapped output.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn rgb48le_to_rgb24_swap_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    const M0: ([u8; 16], [u8; 16]) = swap_masks(0);
    const M1: ([u8; 16], [u8; 16]) = swap_masks(1);
    let blocks = width / 8;
    for i in 0..blocks {
        // SAFETY: as in `rgb48le_to_rgb24_sse2`, then bytes rearranged across the two halves.
        unsafe {
            let s = src.as_ptr().add(i * 48);
            let hi = |k: usize| _mm_srli_epi16::<8>(load(s.add(16 * k)));
            let a = _mm_packus_epi16(hi(0), hi(1));
            let b = _mm_packus_epi16(hi(2), _mm_setzero_si128());
            let pick = |m: ([u8; 16], [u8; 16])| {
                _mm_or_si128(
                    _mm_shuffle_epi8(a, mask(m.0)),
                    _mm_shuffle_epi8(b, mask(m.1)),
                )
            };
            let d = dst.as_mut_ptr().add(i * 24);
            store(d, pick(M0));
            _mm_storel_epi64(d.add(16).cast(), pick(M1));
        }
    }
    blocks * 8
}

/// # Safety
/// SSE2 must be available; `src` holds `width` 2-byte pixels, `dst` `width` bytes.
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn yuyv_luma_sse2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 32 bytes in, 16 out; the even bytes are Y.
        unsafe {
            let s = src.as_ptr().add(i * 32);
            let lo = _mm_set1_epi16(0xFF);
            let y = _mm_packus_epi16(
                _mm_and_si128(load(s), lo),
                _mm_and_si128(load(s.add(16)), lo),
            );
            store(dst.as_mut_ptr().add(i * 16), y);
        }
    }
    blocks * 16
}

/// # Safety
/// AVX2 must be available; as [`yuyv_luma_sse2`].
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn yuyv_luma_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    let blocks = width / 32;
    for i in 0..blocks {
        // SAFETY: 64 bytes in, 32 out. `packus` interleaves the lanes; the permute restores
        // their order.
        unsafe {
            let s = src.as_ptr().add(i * 64);
            let lo = _mm256_set1_epi16(0xFF);
            let a = _mm256_and_si256(_mm256_loadu_si256(s.cast()), lo);
            let b = _mm256_and_si256(_mm256_loadu_si256(s.add(32).cast()), lo);
            let y = _mm256_permute4x64_epi64::<0xD8>(_mm256_packus_epi16(a, b));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32).cast(), y);
        }
    }
    blocks * 32
}

/// Pairwise sums of 16 bytes as 8 u16 lanes.
#[inline(always)]
unsafe fn pair_sums(v: __m128i) -> __m128i {
    // SAFETY: plain SSE2 arithmetic, enabled by the caller.
    unsafe {
        _mm_add_epi16(
            _mm_and_si128(v, _mm_set1_epi16(0xFF)),
            _mm_srli_epi16::<8>(v),
        )
    }
}

/// # Safety
/// SSE2 must be available; `top` and `bottom` hold `2 * width` bytes, `dst` `width`.
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn box2_sse2(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 16;
    for i in 0..blocks {
        // SAFETY: 32 bytes of each row in, 16 out: (sum + 2) >> 2 in 16-bit lanes.
        unsafe {
            let (t, b) = (top.as_ptr().add(i * 32), bottom.as_ptr().add(i * 32));
            let two = _mm_set1_epi16(2);
            let half = |k: usize| {
                let s = _mm_add_epi16(
                    pair_sums(load(t.add(16 * k))),
                    pair_sums(load(b.add(16 * k))),
                );
                _mm_srli_epi16::<2>(_mm_add_epi16(s, two))
            };
            store(
                dst.as_mut_ptr().add(i * 16),
                _mm_packus_epi16(half(0), half(1)),
            );
        }
    }
    blocks * 16
}

/// # Safety
/// AVX2 must be available; as [`box2_sse2`].
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn box2_avx2(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let blocks = width / 32;
    for i in 0..blocks {
        // SAFETY: 64 bytes of each row in, 32 out.
        unsafe {
            let (t, b) = (top.as_ptr().add(i * 64), bottom.as_ptr().add(i * 64));
            let lo = _mm256_set1_epi16(0xFF);
            let sums =
                |v: __m256i| _mm256_add_epi16(_mm256_and_si256(v, lo), _mm256_srli_epi16::<8>(v));
            let half = |k: usize| {
                let tv = _mm256_loadu_si256(t.add(32 * k).cast());
                let bv = _mm256_loadu_si256(b.add(32 * k).cast());
                let s = _mm256_add_epi16(sums(tv), sums(bv));
                _mm256_srli_epi16::<2>(_mm256_add_epi16(s, _mm256_set1_epi16(2)))
            };
            let out = _mm256_permute4x64_epi64::<0xD8>(_mm256_packus_epi16(half(0), half(1)));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32).cast(), out);
        }
    }
    blocks * 32
}

/// Reverse the 8 16-bit lanes of `v`.
#[inline(always)]
unsafe fn rev_u16x8(v: __m128i) -> __m128i {
    // SAFETY: plain SSE2 shuffles, enabled by the caller.
    unsafe {
        let v = _mm_shufflehi_epi16::<0x1B>(_mm_shufflelo_epi16::<0x1B>(v));
        _mm_shuffle_epi32::<0x4E>(v)
    }
}

/// Reverse whole blocks of `LANES` pixels of `bpp` bytes from the start of the source into the
/// end of `dst` with `rev`; returns the source pixels done.
#[inline(always)]
unsafe fn reverse_blocks<const LANES: usize>(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
    rev: impl Fn(*const u8, *mut u8),
) -> usize {
    let blocks = width / LANES;
    for i in 0..blocks {
        let out = (width - LANES * (i + 1)) * bpp;
        // Pointers to one block in each row, inside both per the caller.
        rev(unsafe { src.as_ptr().add(i * LANES * bpp) }, unsafe {
            dst.as_mut_ptr().add(out)
        });
    }
    blocks * LANES
}

/// # Safety
/// SSE2 must be available; both rows hold `width` pixels of `bpp` bytes (1, 2 or 4).
#[target_feature(enable = "sse2")]
pub(in crate::simd) unsafe fn reverse_sse2(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
) -> usize {
    // SAFETY (all closures): 16 bytes readable at `s` and writable at `d`.
    unsafe {
        match bpp {
            1 => reverse_blocks::<16>(src, dst, width, 1, |s, d| {
                let v = rev_u16x8(load(s));
                // Swap the bytes of each 16-bit lane.
                store(
                    d,
                    _mm_or_si128(_mm_slli_epi16::<8>(v), _mm_srli_epi16::<8>(v)),
                );
            }),
            2 => reverse_blocks::<8>(src, dst, width, 2, |s, d| store(d, rev_u16x8(load(s)))),
            4 => reverse_blocks::<4>(src, dst, width, 4, |s, d| {
                store(d, _mm_shuffle_epi32::<0x1B>(load(s)))
            }),
            _ => 0,
        }
    }
}

const REV_BYTES: [u8; 16] = [15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0];

/// # Safety
/// SSSE3 must be available; both rows hold `width` bytes.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn reverse_u8_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
    // SAFETY: 16 bytes readable at `s` and writable at `d`.
    unsafe {
        reverse_blocks::<16>(src, dst, width, 1, |s, d| {
            store(d, _mm_shuffle_epi8(load(s), mask(REV_BYTES)))
        })
    }
}

/// Reverse 3-byte pixels, 5 per 16-byte vector. Blocks are stored from the start of `dst`
/// towards the end, so each 16th spilled byte lands on the next block's first byte before that
/// block is written. Block 0 would spill past the row, so it and any pixels past the last full
/// block are left to the scalar kernel: returns the first and last source pixels done.
///
/// # Safety
/// SSSE3 must be available; both rows hold `width` 3-byte pixels.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn reverse_rgb_ssse3(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> (usize, usize) {
    const REV_5PX: [u8; 16] = [12, 13, 14, 9, 10, 11, 6, 7, 8, 3, 4, 5, 0, 1, 2, 0x80];
    if width < 10 {
        return (0, 0);
    }
    // The last block whose 16-byte load stays inside the source row.
    let last = (width / 5 - 1).min((width * 3 - 16) / 15);
    if last < 1 {
        return (0, 0);
    }
    // Source block k (pixels 5k..5k+5) goes to destination pixels width-5k-5.. ; blocks with
    // a larger k sit earlier in `dst`, so go from k = last down to 1.
    for k in (1..=last).rev() {
        // SAFETY: 15k + 16 <= 3 * width source bytes; the destination store ends at pixel
        // width-5k's first byte, inside the row for k >= 1.
        unsafe {
            let v = _mm_shuffle_epi8(load(src.as_ptr().add(15 * k)), mask(REV_5PX));
            store(dst.as_mut_ptr().add((width - 5 * k - 5) * 3), v);
        }
    }
    (5, 5 * (last + 1))
}
