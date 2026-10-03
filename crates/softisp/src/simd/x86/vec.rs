//! One vector vocabulary for SSE2 (`__m128i`) and AVX2 (`__m256i`), so each kernel body is
//! written once and instantiated in a `#[target_feature]` wrapper per instruction set. All
//! operations are lane-wise within 128-bit halves except where noted; methods are only called
//! from wrappers that enabled the matching feature.

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

pub(super) trait Vx: Copy {
    /// Bytes per vector.
    const BYTES: usize;
    /// # Safety
    /// `BYTES` readable bytes at `p`.
    unsafe fn load(p: *const u8) -> Self;
    /// # Safety
    /// `BYTES` writable bytes at `p`.
    unsafe fn store(p: *mut u8, v: Self);
    /// # Safety
    /// `BYTES / 2` writable bytes at `p` (the low half; for AVX2 after [`Vx::fix_pack`]).
    unsafe fn store_half(p: *mut u8, v: Self);
    unsafe fn splat16(v: i16) -> Self;
    unsafe fn splat32(v: i32) -> Self;
    unsafe fn splat64(v: i64) -> Self;
    /// `pshufb` with the same 16-byte mask in every 128-bit half (SSSE3).
    unsafe fn shuffle_bytes(v: Self, mask: &[u8; 16]) -> Self;
    unsafe fn zero() -> Self;
    unsafe fn add16(a: Self, b: Self) -> Self;
    unsafe fn adds_u16(a: Self, b: Self) -> Self;
    unsafe fn subs_u16(a: Self, b: Self) -> Self;
    unsafe fn avg_u16(a: Self, b: Self) -> Self;
    unsafe fn mulhi_u16(a: Self, b: Self) -> Self;
    unsafe fn mullo16(a: Self, b: Self) -> Self;
    unsafe fn madd16(a: Self, b: Self) -> Self;
    /// `(a b + 2^14) >> 15` (SSSE3).
    unsafe fn mulhrs(a: Self, b: Self) -> Self;
    unsafe fn adds_i16(a: Self, b: Self) -> Self;
    unsafe fn add32(a: Self, b: Self) -> Self;
    unsafe fn srai32<const N: i32>(a: Self) -> Self;
    unsafe fn srli16<const N: i32>(a: Self) -> Self;
    unsafe fn srai16<const N: i32>(a: Self) -> Self;
    unsafe fn slli16<const N: i32>(a: Self) -> Self;
    unsafe fn sll16(a: Self, count: __m128i) -> Self;
    unsafe fn and(a: Self, b: Self) -> Self;
    unsafe fn andnot(a: Self, b: Self) -> Self;
    unsafe fn or(a: Self, b: Self) -> Self;
    unsafe fn min_i16(a: Self, b: Self) -> Self;
    unsafe fn cmplt_i16(a: Self, b: Self) -> Self;
    unsafe fn max_i16(a: Self, b: Self) -> Self;
    unsafe fn unpacklo16(a: Self, b: Self) -> Self;
    unsafe fn unpackhi16(a: Self, b: Self) -> Self;
    unsafe fn unpacklo8(a: Self, b: Self) -> Self;
    unsafe fn unpackhi8(a: Self, b: Self) -> Self;
    /// Signed saturation of i32 to i16, per 128-bit half.
    unsafe fn packs32(a: Self, b: Self) -> Self;
    /// Unsigned saturation of i16 to u8, per 128-bit half.
    unsafe fn packus16(a: Self, b: Self) -> Self;
    /// Put the halves of a per-half pack of `a` then `b` back in `a`, `b` order (AVX2).
    unsafe fn fix_pack(v: Self) -> Self;

    /// `(a & mask) | (b & !mask)`.
    #[inline(always)]
    unsafe fn select(mask: Self, a: Self, b: Self) -> Self {
        unsafe { Self::or(Self::and(mask, a), Self::andnot(mask, b)) }
    }

    /// `min(a, max)` for unsigned 16-bit lanes (SSE2 has no `pminuw`).
    #[inline(always)]
    unsafe fn min_u16(a: Self, max: u16) -> Self {
        unsafe {
            let bias = Self::splat16((0xFFFF - max) as i16);
            Self::subs_u16(Self::adds_u16(a, bias), bias)
        }
    }
}

impl Vx for __m128i {
    const BYTES: usize = 16;
    #[inline(always)]
    unsafe fn load(p: *const u8) -> Self {
        // SAFETY: the caller guarantees 16 readable bytes.
        unsafe { _mm_loadu_si128(p.cast()) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut u8, v: Self) {
        // SAFETY: the caller guarantees 16 writable bytes.
        unsafe { _mm_storeu_si128(p.cast(), v) }
    }
    #[inline(always)]
    unsafe fn store_half(p: *mut u8, v: Self) {
        // SAFETY: the caller guarantees 8 writable bytes.
        unsafe { _mm_storel_epi64(p.cast(), v) }
    }
    #[inline(always)]
    unsafe fn splat16(v: i16) -> Self {
        unsafe { _mm_set1_epi16(v) }
    }
    #[inline(always)]
    unsafe fn splat32(v: i32) -> Self {
        unsafe { _mm_set1_epi32(v) }
    }
    #[inline(always)]
    unsafe fn splat64(v: i64) -> Self {
        unsafe { _mm_set1_epi64x(v) }
    }
    #[inline(always)]
    unsafe fn shuffle_bytes(v: Self, mask: &[u8; 16]) -> Self {
        // SAFETY: an unaligned load of the 16-byte mask.
        unsafe { _mm_shuffle_epi8(v, _mm_loadu_si128(mask.as_ptr().cast())) }
    }
    #[inline(always)]
    unsafe fn zero() -> Self {
        unsafe { _mm_setzero_si128() }
    }
    #[inline(always)]
    unsafe fn add16(a: Self, b: Self) -> Self {
        unsafe { _mm_add_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn adds_u16(a: Self, b: Self) -> Self {
        unsafe { _mm_adds_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn subs_u16(a: Self, b: Self) -> Self {
        unsafe { _mm_subs_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn avg_u16(a: Self, b: Self) -> Self {
        unsafe { _mm_avg_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn mulhi_u16(a: Self, b: Self) -> Self {
        unsafe { _mm_mulhi_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn mullo16(a: Self, b: Self) -> Self {
        unsafe { _mm_mullo_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn madd16(a: Self, b: Self) -> Self {
        unsafe { _mm_madd_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn mulhrs(a: Self, b: Self) -> Self {
        unsafe { _mm_mulhrs_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn adds_i16(a: Self, b: Self) -> Self {
        unsafe { _mm_adds_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn add32(a: Self, b: Self) -> Self {
        unsafe { _mm_add_epi32(a, b) }
    }
    #[inline(always)]
    unsafe fn srai32<const N: i32>(a: Self) -> Self {
        unsafe { _mm_srai_epi32::<N>(a) }
    }
    #[inline(always)]
    unsafe fn srli16<const N: i32>(a: Self) -> Self {
        unsafe { _mm_srli_epi16::<N>(a) }
    }
    #[inline(always)]
    unsafe fn srai16<const N: i32>(a: Self) -> Self {
        unsafe { _mm_srai_epi16::<N>(a) }
    }
    #[inline(always)]
    unsafe fn slli16<const N: i32>(a: Self) -> Self {
        unsafe { _mm_slli_epi16::<N>(a) }
    }
    #[inline(always)]
    unsafe fn sll16(a: Self, count: __m128i) -> Self {
        unsafe { _mm_sll_epi16(a, count) }
    }
    #[inline(always)]
    unsafe fn and(a: Self, b: Self) -> Self {
        unsafe { _mm_and_si128(a, b) }
    }
    #[inline(always)]
    unsafe fn andnot(a: Self, b: Self) -> Self {
        unsafe { _mm_andnot_si128(a, b) }
    }
    #[inline(always)]
    unsafe fn or(a: Self, b: Self) -> Self {
        unsafe { _mm_or_si128(a, b) }
    }
    #[inline(always)]
    unsafe fn min_i16(a: Self, b: Self) -> Self {
        unsafe { _mm_min_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn cmplt_i16(a: Self, b: Self) -> Self {
        unsafe { _mm_cmplt_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn max_i16(a: Self, b: Self) -> Self {
        unsafe { _mm_max_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn unpacklo16(a: Self, b: Self) -> Self {
        unsafe { _mm_unpacklo_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn unpackhi16(a: Self, b: Self) -> Self {
        unsafe { _mm_unpackhi_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn unpacklo8(a: Self, b: Self) -> Self {
        unsafe { _mm_unpacklo_epi8(a, b) }
    }
    #[inline(always)]
    unsafe fn unpackhi8(a: Self, b: Self) -> Self {
        unsafe { _mm_unpackhi_epi8(a, b) }
    }
    #[inline(always)]
    unsafe fn packs32(a: Self, b: Self) -> Self {
        unsafe { _mm_packs_epi32(a, b) }
    }
    #[inline(always)]
    unsafe fn packus16(a: Self, b: Self) -> Self {
        unsafe { _mm_packus_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn fix_pack(v: Self) -> Self {
        v
    }
}

impl Vx for __m256i {
    const BYTES: usize = 32;
    #[inline(always)]
    unsafe fn load(p: *const u8) -> Self {
        // SAFETY: the caller guarantees 32 readable bytes.
        unsafe { _mm256_loadu_si256(p.cast()) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut u8, v: Self) {
        // SAFETY: the caller guarantees 32 writable bytes.
        unsafe { _mm256_storeu_si256(p.cast(), v) }
    }
    #[inline(always)]
    unsafe fn store_half(p: *mut u8, v: Self) {
        // SAFETY: the caller guarantees 16 writable bytes.
        unsafe { _mm_storeu_si128(p.cast(), _mm256_castsi256_si128(v)) }
    }
    #[inline(always)]
    unsafe fn splat16(v: i16) -> Self {
        unsafe { _mm256_set1_epi16(v) }
    }
    #[inline(always)]
    unsafe fn splat32(v: i32) -> Self {
        unsafe { _mm256_set1_epi32(v) }
    }
    #[inline(always)]
    unsafe fn splat64(v: i64) -> Self {
        unsafe { _mm256_set1_epi64x(v) }
    }
    #[inline(always)]
    unsafe fn shuffle_bytes(v: Self, mask: &[u8; 16]) -> Self {
        // SAFETY: an unaligned load of the 16-byte mask.
        unsafe {
            let m = _mm256_broadcastsi128_si256(_mm_loadu_si128(mask.as_ptr().cast()));
            _mm256_shuffle_epi8(v, m)
        }
    }
    #[inline(always)]
    unsafe fn zero() -> Self {
        unsafe { _mm256_setzero_si256() }
    }
    #[inline(always)]
    unsafe fn add16(a: Self, b: Self) -> Self {
        unsafe { _mm256_add_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn adds_u16(a: Self, b: Self) -> Self {
        unsafe { _mm256_adds_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn subs_u16(a: Self, b: Self) -> Self {
        unsafe { _mm256_subs_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn avg_u16(a: Self, b: Self) -> Self {
        unsafe { _mm256_avg_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn mulhi_u16(a: Self, b: Self) -> Self {
        unsafe { _mm256_mulhi_epu16(a, b) }
    }
    #[inline(always)]
    unsafe fn mullo16(a: Self, b: Self) -> Self {
        unsafe { _mm256_mullo_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn madd16(a: Self, b: Self) -> Self {
        unsafe { _mm256_madd_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn mulhrs(a: Self, b: Self) -> Self {
        unsafe { _mm256_mulhrs_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn adds_i16(a: Self, b: Self) -> Self {
        unsafe { _mm256_adds_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn add32(a: Self, b: Self) -> Self {
        unsafe { _mm256_add_epi32(a, b) }
    }
    #[inline(always)]
    unsafe fn srai32<const N: i32>(a: Self) -> Self {
        unsafe { _mm256_srai_epi32::<N>(a) }
    }
    #[inline(always)]
    unsafe fn srli16<const N: i32>(a: Self) -> Self {
        unsafe { _mm256_srli_epi16::<N>(a) }
    }
    #[inline(always)]
    unsafe fn srai16<const N: i32>(a: Self) -> Self {
        unsafe { _mm256_srai_epi16::<N>(a) }
    }
    #[inline(always)]
    unsafe fn slli16<const N: i32>(a: Self) -> Self {
        unsafe { _mm256_slli_epi16::<N>(a) }
    }
    #[inline(always)]
    unsafe fn sll16(a: Self, count: __m128i) -> Self {
        unsafe { _mm256_sll_epi16(a, count) }
    }
    #[inline(always)]
    unsafe fn and(a: Self, b: Self) -> Self {
        unsafe { _mm256_and_si256(a, b) }
    }
    #[inline(always)]
    unsafe fn andnot(a: Self, b: Self) -> Self {
        unsafe { _mm256_andnot_si256(a, b) }
    }
    #[inline(always)]
    unsafe fn or(a: Self, b: Self) -> Self {
        unsafe { _mm256_or_si256(a, b) }
    }
    #[inline(always)]
    unsafe fn min_i16(a: Self, b: Self) -> Self {
        unsafe { _mm256_min_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn cmplt_i16(a: Self, b: Self) -> Self {
        unsafe { _mm256_cmpgt_epi16(b, a) }
    }
    #[inline(always)]
    unsafe fn max_i16(a: Self, b: Self) -> Self {
        unsafe { _mm256_max_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn unpacklo16(a: Self, b: Self) -> Self {
        unsafe { _mm256_unpacklo_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn unpackhi16(a: Self, b: Self) -> Self {
        unsafe { _mm256_unpackhi_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn unpacklo8(a: Self, b: Self) -> Self {
        unsafe { _mm256_unpacklo_epi8(a, b) }
    }
    #[inline(always)]
    unsafe fn unpackhi8(a: Self, b: Self) -> Self {
        unsafe { _mm256_unpackhi_epi8(a, b) }
    }
    #[inline(always)]
    unsafe fn packs32(a: Self, b: Self) -> Self {
        unsafe { _mm256_packs_epi32(a, b) }
    }
    #[inline(always)]
    unsafe fn packus16(a: Self, b: Self) -> Self {
        unsafe { _mm256_packus_epi16(a, b) }
    }
    #[inline(always)]
    unsafe fn fix_pack(v: Self) -> Self {
        unsafe { _mm256_permute4x64_epi64::<0b11_01_10_00>(v) }
    }
}
