//! The tone curve with AVX2 byte shuffles, bit for bit the table's result.
//!
//! The 4096-entry table interpolates 257 nodes: `out = (n[i] (16 - f) + n[i + 1] f + 8) >> 4`
//! with `i = x >> 4`, `f = x & 15`. Both node lookups are 256-entry byte lookups, done with
//! `vpshufb` (16 entries) in a cascade: sixteen-entry tables `T[k] ^ T[k - 1]` are looked up
//! with the index lowered by 16 at each step, so every step below the index's own block adds a
//! difference that telescopes to `T[block]` and every step above it has the index negative
//! (top bit set), which `vpshufb` turns into 0. The lower and upper 128 entries run as two
//! cascades (the upper one on `i ^ 0x80`) and a blend on the index's top bit picks between
//! them. The interpolation is one `vpmaddubsw` on the (`n[i]`, `n[i + 1]`) byte pairs with the
//! (`16 - f`, `f`) weight pairs. 37 shuffles per 32 pixels against 32 scalar table loads and
//! byte stores: 2x faster on Zen 3.

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// Cascade tables of one 256-entry lookup: `[k]` for `k < 8` the lower half's differences,
/// `[8 + k]` the upper half's, each 16 bytes repeated for both 128-bit lanes.
pub(in crate::simd) type Cascade = [[u8; 32]; 16];

/// The cascade tables of `table` (256 entries).
pub(in crate::simd) fn cascade(table: impl Fn(usize) -> u8) -> Cascade {
    core::array::from_fn(|t| {
        let (half, k) = (t / 8, t % 8);
        core::array::from_fn(|j| {
            let at = |k: usize| table(128 * half + 16 * k + j % 16);
            if k == 0 { at(0) } else { at(k) ^ at(k - 1) }
        })
    })
}

/// Node indices of 32 samples, lane-interleaved as `vpackuswb` leaves them, and the samples
/// (clamped to 4095).
///
/// # Safety
/// AVX2 must be available; `p` points at 32 samples.
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn indices(p: *const u16) -> (__m256i, [__m256i; 2]) {
    // SAFETY: as the caller.
    unsafe {
        let max = _mm256_set1_epi16(4095);
        let x0 = _mm256_min_epu16(_mm256_loadu_si256(p as *const __m256i), max);
        let x1 = _mm256_min_epu16(_mm256_loadu_si256(p.add(16) as *const __m256i), max);
        // [x0 0..8, x1 0..8 | x0 8..16, x1 8..16]
        let idx = _mm256_packus_epi16(_mm256_srli_epi16(x0, 4), _mm256_srli_epi16(x1, 4));
        (idx, [x0, x1])
    }
}

/// Interpolates 32 samples from their nodes `n0 = n[i]` and `n1 = n[i + 1]` and stores them.
///
/// # Safety
/// AVX2 must be available; `dst` has room for 32 bytes.
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn finish(x: [__m256i; 2], n0: __m256i, n1: __m256i, dst: *mut u8) {
    // SAFETY: as the caller.
    unsafe {
        let low4 = _mm256_set1_epi16(15);
        let sixteen = _mm256_set1_epi16(16);
        let eight = _mm256_set1_epi16(8);
        // (n[i], n[i + 1]) pairs in x[0]'s and x[1]'s order.
        let p = [_mm256_unpacklo_epi8(n0, n1), _mm256_unpackhi_epi8(n0, n1)];
        let mut v = [_mm256_setzero_si256(); 2];
        for j in 0..2 {
            // Weights (16 - f, f) as byte pairs: 16 - f + 256 f.
            let f = _mm256_and_si256(x[j], low4);
            let w = _mm256_add_epi16(_mm256_sub_epi16(_mm256_slli_epi16(f, 8), f), sixteen);
            v[j] = _mm256_srli_epi16(_mm256_add_epi16(_mm256_maddubs_epi16(p[j], w), eight), 4);
        }
        let out = _mm256_permute4x64_epi64(_mm256_packus_epi16(v[0], v[1]), 0b11_01_10_00);
        _mm256_storeu_si256(dst as *mut __m256i, out);
    }
}

/// One node array's entries for the indices of 64 samples at `src` into `out`.
///
/// # Safety
/// AVX2 must be available; 64 samples at `src`, room for 64 bytes at `out`.
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn nodes64(src: *const u16, c: &Cascade, levels: usize, out: *mut u8) {
    // SAFETY: as the caller; the tables are 32 bytes each.
    unsafe {
        let step = _mm256_set1_epi8(16);
        let flip = _mm256_set1_epi8(-128);
        let t = c.as_ptr() as *const __m256i;
        let (ia, _) = indices(src);
        let (ib, _) = indices(src.add(32));
        let z = _mm256_setzero_si256();
        let (mut la, mut ha, mut lb, mut hb) = (z, z, z, z);
        let (mut a, mut b) = (ia, ib);
        for k in 0..levels {
            let lo = _mm256_loadu_si256(t.add(k));
            let hi = _mm256_loadu_si256(t.add(8 + k));
            // The upper half's index: `i - 128 - 16 k`.
            let (ah, bh) = (_mm256_xor_si256(a, flip), _mm256_xor_si256(b, flip));
            la = _mm256_xor_si256(la, _mm256_shuffle_epi8(lo, a));
            ha = _mm256_xor_si256(ha, _mm256_shuffle_epi8(hi, ah));
            lb = _mm256_xor_si256(lb, _mm256_shuffle_epi8(lo, b));
            hb = _mm256_xor_si256(hb, _mm256_shuffle_epi8(hi, bh));
            a = _mm256_sub_epi8(a, step);
            b = _mm256_sub_epi8(b, step);
        }
        // The index's top bit picks the upper half's result.
        _mm256_storeu_si256(out as *mut __m256i, _mm256_blendv_epi8(la, ha, ia));
        _mm256_storeu_si256(out.add(32) as *mut __m256i, _mm256_blendv_epi8(lb, hb, ib));
    }
}

/// `dst[x] = table(src[x])` for 64 pixels at a time; returns the pixels done.
///
/// Two blocks of 32 share each table load (the shuffles' tables come from memory; with one
/// block the loads limited the loop), and each node array is a pass of its own over the
/// row's 64-sample groups, its level loop opaque to the compiler: with everything unrolled
/// it kept the 32 tables on the stack and spilled the 16 indices (1.4x slower).
///
/// # Safety
/// AVX2 must be available; `src` and `dst` hold `width` samples.
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn lut_avx2(
    src: &[u16],
    dst: &mut [u8],
    nodes: &[Cascade; 2],
    width: usize,
) -> usize {
    let n = width / 64 * 64;
    assert!(src.len() >= n && dst.len() >= n);
    let levels = core::hint::black_box(8usize);
    let mut n0 = [0u8; 64];
    let mut n1 = [0u8; 64];
    // SAFETY: loads of 64 samples and stores of 64 bytes at `i` with `i + 64 <= n`.
    unsafe {
        for i in (0..n).step_by(64) {
            let s = src.as_ptr().add(i);
            nodes64(s, &nodes[0], levels, n0.as_mut_ptr());
            nodes64(s, &nodes[1], levels, n1.as_mut_ptr());
            for j in [0, 32] {
                let (_, x) = indices(s.add(j));
                let p = |a: &[u8; 64]| _mm256_loadu_si256(a.as_ptr().add(j) as *const __m256i);
                finish(x, p(&n0), p(&n1), dst.as_mut_ptr().add(i + j));
            }
        }
    }
    n
}

/// Byte tables of the octave of `v = x + 32` (as `2 octave`, the low byte of a lookup into
/// 16-bit tables): by `v >> 4` below 256, by `v >> 8` from 256 (entry 0 is 4096 and up).
const OCTAVE_LO: [u8; 16] = [0, 0, 0, 0, 2, 2, 2, 2, 4, 4, 4, 4, 4, 4, 4, 4];
const OCTAVE_HI: [u8; 16] = [12, 6, 8, 8, 10, 10, 10, 10, 12, 12, 12, 12, 12, 12, 12, 12];
/// `2^(14 - e)` for octave `e - 5`.
const SCALE: [i16; 8] = [512, 256, 128, 64, 32, 16, 8, 0];

/// [`crate::simd::poly::PolyTone`] for 32 pixels at a time; returns the pixels done. Each
/// coefficient is a `vpshufb` per half octave and a blend.
///
/// # Safety
/// AVX2 must be available; `src` and `dst` hold `width` samples.
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn poly_avx2(
    src: &[u16],
    dst: &mut [u8],
    c: &[[[i16; 8]; 2]; 3],
    width: usize,
) -> usize {
    let n = width / 32 * 32;
    assert!(src.len() >= n && dst.len() >= n);
    // No closures here: they would not inherit `target_feature`.
    // SAFETY: loads of 32 samples and stores of 32 bytes at `i` with `i + 32 <= n`; the
    // tables are 16 and 32 bytes.
    unsafe {
        let lo_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(OCTAVE_LO.as_ptr().cast()));
        let hi_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(OCTAVE_HI.as_ptr().cast()));
        let scale_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(SCALE.as_ptr().cast()));
        // Each coefficient's tables by octave, for the first and the second half.
        let mut k = [[_mm256_setzero_si256(); 2]; 3];
        for (k, c) in k.iter_mut().zip(c) {
            for (k, c) in k.iter_mut().zip(c) {
                *k = _mm256_broadcastsi128_si256(_mm_loadu_si128(c.as_ptr().cast()));
            }
        }
        let max = _mm256_set1_epi16(4095);
        let offset = _mm256_set1_epi16(32);
        // High byte of a lookup index: a zero result.
        let zero_hi = _mm256_set1_epi16(-32768);
        let pair = _mm256_set1_epi16(0x0101);
        let odd = _mm256_set1_epi16(0x0100);
        let below256 = _mm256_set1_epi16(255);
        let one = _mm256_set1_epi16(1 << 14);
        let mid = _mm256_set1_epi16((1 << 13) - 1);
        let round = _mm256_set1_epi16(1024);
        for i in (0..n).step_by(32) {
            let mut r = [_mm256_setzero_si256(); 2];
            for (j, r) in r.iter_mut().enumerate() {
                let p = src.as_ptr().add(i + 16 * j) as *const __m256i;
                let v = _mm256_add_epi16(_mm256_min_epu16(_mm256_loadu_si256(p), max), offset);
                let lo = _mm256_or_si256(_mm256_srli_epi16(v, 4), zero_hi);
                let hi = _mm256_or_si256(_mm256_srli_epi16(v, 8), zero_hi);
                // 2 octave in the low byte.
                let k2 = _mm256_blendv_epi8(
                    _mm256_shuffle_epi8(lo_t, lo),
                    _mm256_shuffle_epi8(hi_t, hi),
                    _mm256_cmpgt_epi16(v, below256),
                );
                let at = _mm256_add_epi16(_mm256_mullo_epi16(k2, pair), odd);
                let scale = _mm256_shuffle_epi8(scale_t, at);
                let t = _mm256_sub_epi16(_mm256_mullo_epi16(v, scale), one);
                // The second half of the octave (t is at most 16632: a signed compare).
                let second = _mm256_cmpgt_epi16(t, mid);
                let mut kc = [_mm256_setzero_si256(); 3];
                for (kc, k) in kc.iter_mut().zip(&k) {
                    *kc = _mm256_blendv_epi8(
                        _mm256_shuffle_epi8(k[0], at),
                        _mm256_shuffle_epi8(k[1], at),
                        second,
                    );
                }
                let inner = _mm256_adds_epi16(kc[1], _mm256_mulhrs_epi16(kc[2], t));
                let y = _mm256_adds_epi16(kc[0], _mm256_mulhrs_epi16(inner, t));
                *r = _mm256_mulhrs_epi16(y, round);
            }
            // [r0 0..8, r1 0..8 | r0 8..16, r1 8..16] as bytes, clamped to 0..=255.
            let out = _mm256_permute4x64_epi64(_mm256_packus_epi16(r[0], r[1]), 0b11_01_10_00);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, out);
        }
    }
    n
}
