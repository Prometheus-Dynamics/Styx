//! The tone curve with AVX-512 VBMI byte permutes, bit for bit the table's result (written,
//! untested on an AVX-512 host: the conformance tests run it where one is detected).
//!
//! The same interpolation of 257 nodes as the AVX2 leaf (`out = (n[i] (16 - f) + n[i + 1] f +
//! 8) >> 4`, `i = x >> 4`, `f = x & 15`), but each 256-entry lookup is two `vpermi2b` (128
//! entries each, from two registers) and a blend on the index's top bit, 64 pixels at a time:
//! four permutes and two blends per 64 pixels for both node arrays, against the AVX2 leaf's 74
//! shuffles.

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// [`crate::isp::scalar::lut_row`] by the table's 257 `nodes`, 64 pixels at a time; returns
/// the pixels done.
///
/// # Safety
/// AVX-512 F, BW and VBMI must be available; `src` and `dst` hold `width` samples.
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
pub(in crate::isp) unsafe fn lut_avx512vbmi(
    src: &[u16],
    dst: &mut [u8],
    nodes: &[u8; 257],
    width: usize,
) -> usize {
    let n = width / 64 * 64;
    assert!(src.len() >= n && dst.len() >= n);
    // SAFETY: the node loads read 64 bytes at offsets up to 193 of 257; the sample loads and
    // byte stores are at `i` and `i + 32` with `i + 64 <= n`.
    unsafe {
        let at = |o: usize| _mm512_loadu_si512(nodes.as_ptr().add(o).cast());
        // n[i] and n[i + 1]: the lower and upper 128 entries, two registers each.
        let a = [at(0), at(64), at(128), at(192)];
        let b = [at(1), at(65), at(129), at(193)];
        let max = _mm512_set1_epi16(4095);
        let low4 = _mm512_set1_epi16(15);
        let half = _mm512_set1_epi16(8);
        for i in (0..n).step_by(64) {
            let s = src.as_ptr().add(i);
            let x = [
                _mm512_min_epu16(_mm512_loadu_si512(s.cast()), max),
                _mm512_min_epu16(_mm512_loadu_si512(s.add(32).cast()), max),
            ];
            // The 64 node indices as bytes.
            let idx = _mm512_inserti64x4::<1>(
                _mm512_castsi256_si512(_mm512_cvtepi16_epi8(_mm512_srli_epi16::<4>(x[0]))),
                _mm512_cvtepi16_epi8(_mm512_srli_epi16::<4>(x[1])),
            );
            let upper = _mm512_movepi8_mask(idx);
            let na = _mm512_mask_blend_epi8(
                upper,
                _mm512_permutex2var_epi8(a[0], idx, a[1]),
                _mm512_permutex2var_epi8(a[2], idx, a[3]),
            );
            let nb = _mm512_mask_blend_epi8(
                upper,
                _mm512_permutex2var_epi8(b[0], idx, b[1]),
                _mm512_permutex2var_epi8(b[2], idx, b[3]),
            );
            let mut out = [_mm256_setzero_si256(); 2];
            for (h, out) in out.iter_mut().enumerate() {
                let wide = |v: __m512i| {
                    _mm512_cvtepu8_epi16(if h == 0 {
                        _mm512_castsi512_si256(v)
                    } else {
                        _mm512_extracti64x4_epi64::<1>(v)
                    })
                };
                let (va, vb) = (wide(na), wide(nb));
                let f = _mm512_and_si512(x[h], low4);
                // 16 a + (b - a) f + 8 = a (16 - f) + b f + 8: at most 4088, never negative.
                let sum = _mm512_add_epi16(
                    _mm512_add_epi16(_mm512_slli_epi16::<4>(va), half),
                    _mm512_mullo_epi16(_mm512_sub_epi16(vb, va), f),
                );
                *out = _mm512_cvtepi16_epi8(_mm512_srli_epi16::<4>(sum));
            }
            _mm512_storeu_si512(
                dst.as_mut_ptr().add(i).cast(),
                _mm512_inserti64x4::<1>(_mm512_castsi256_si512(out[0]), out[1]),
            );
        }
    }
    n
}
