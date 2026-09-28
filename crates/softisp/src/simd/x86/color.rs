//! Colour-domain kernel bodies (matrix, narrowing, YCbCr) over [`Vx`], instantiated for SSE2 and
//! AVX2, plus the SSSE3 byte-shuffle kernels (RGB interleave, packed RAW unpacking).

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use super::vec::Vx;
use crate::simd::YuvCoeffs;

#[inline(always)]
unsafe fn pair<V: Vx>(lo: i16, hi: i16) -> V {
    unsafe { V::splat32((lo as u16 as u32 | (hi as u16 as u32) << 16) as i32) }
}

#[inline(always)]
pub(super) unsafe fn ccm<V: Vx>(planes: [&mut [u16]; 3], m: &[i16; 9], width: usize) -> usize {
    let [r, g, b] = planes;
    let n = V::BYTES / 2;
    let mut x = 0;
    // SAFETY: loads and stores of `n` lanes at `x` with `x + n <= width`.
    unsafe {
        let one = V::splat16(1);
        // No closures, `array::from_fn` or `map` in these bodies: they do not inherit the
        // wrapper's `#[target_feature]`, so intrinsics inside them can stay out-of-line calls.
        let w_rg = [pair::<V>(m[0], m[1]), pair(m[3], m[4]), pair(m[6], m[7])];
        let w_b = [pair::<V>(m[2], 512), pair(m[5], 512), pair(m[8], 512)];
        let (lo_max, hi_max) = (V::zero(), V::splat16(4095));
        while x + n <= width {
            let rv = V::load(r.as_ptr().add(x).cast());
            let gv = V::load(g.as_ptr().add(x).cast());
            let bv = V::load(b.as_ptr().add(x).cast());
            let (rg_lo, rg_hi) = (V::unpacklo16(rv, gv), V::unpackhi16(rv, gv));
            let (b_lo, b_hi) = (V::unpacklo16(bv, one), V::unpackhi16(bv, one));
            let rows = [rg_lo, rg_hi, b_lo, b_hi];
            let o0 = ccm_out(rows, w_rg[0], w_b[0], lo_max, hi_max);
            let o1 = ccm_out(rows, w_rg[1], w_b[1], lo_max, hi_max);
            let o2 = ccm_out(rows, w_rg[2], w_b[2], lo_max, hi_max);
            V::store(r.as_mut_ptr().add(x).cast(), o0);
            V::store(g.as_mut_ptr().add(x).cast(), o1);
            V::store(b.as_mut_ptr().add(x).cast(), o2);
            x += n;
        }
    }
    x
}

/// One output channel of the matrix from interleaved (R, G) and (B, 1) lanes.
#[inline(always)]
unsafe fn ccm_out<V: Vx>([rg_lo, rg_hi, b_lo, b_hi]: [V; 4], w_rg: V, w_b: V, lo: V, hi: V) -> V {
    unsafe {
        let l = V::add32(V::madd16(rg_lo, w_rg), V::madd16(b_lo, w_b));
        let h = V::add32(V::madd16(rg_hi, w_rg), V::madd16(b_hi, w_b));
        let v = V::packs32(V::srai32::<10>(l), V::srai32::<10>(h));
        V::min_i16(V::max_i16(v, lo), hi)
    }
}

/// `sum(p[i] k[i]) + round` in 16-bit lanes, then `>> 8` (logical) plus `off`, or `>> 7`
/// (arithmetic) plus `off` when `signed`.
#[inline(always)]
unsafe fn dot3<V: Vx>(p: [V; 3], k: &[V; 3], round: V, off: V, signed: bool) -> V {
    unsafe {
        let s = V::add16(
            V::add16(V::mullo16(p[0], k[0]), V::mullo16(p[1], k[1])),
            V::add16(V::mullo16(p[2], k[2]), round),
        );
        let s = if signed {
            V::srai16::<7>(s)
        } else {
            V::srli16::<8>(s)
        };
        V::add16(s, off)
    }
}

/// Rounded means of 2x2 blocks of bytes from two rows, as 16-bit lanes.
#[inline(always)]
unsafe fn mean4<V: Vx>(top: *const u8, bottom: *const u8, low: V, two: V) -> V {
    // SAFETY: the callers guarantee a vector of bytes at both pointers.
    unsafe {
        let (t, b) = (V::load(top), V::load(bottom));
        let s = V::add16(
            V::add16(V::and(t, low), V::srli16::<8>(t)),
            V::add16(V::add16(V::and(b, low), V::srli16::<8>(b)), two),
        );
        V::srli16::<2>(s)
    }
}

#[inline(always)]
pub(super) unsafe fn narrow<V: Vx>(src: &[u16], dst: &mut [u8], width: usize) -> usize {
    let n = V::BYTES;
    let mut x = 0;
    // SAFETY: `n` samples (two vectors) read and `n` bytes written at `x`, `x + n <= width`.
    unsafe {
        while x + n <= width {
            let a = V::srli16::<4>(V::load(src.as_ptr().add(x).cast()));
            let b = V::srli16::<4>(V::load(src.as_ptr().add(x + n / 2).cast()));
            V::store(dst.as_mut_ptr().add(x), V::fix_pack(V::packus16(a, b)));
            x += n;
        }
    }
    x
}

#[inline(always)]
pub(super) unsafe fn rgb_to_y<V: Vx>(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> usize {
    let n = V::BYTES;
    let mut x = 0;
    // SAFETY: `n` bytes of each plane read and written at `x`, `x + n <= width`.
    unsafe {
        let z = V::zero();
        let k = [
            V::splat16(c.y[0] as i16),
            V::splat16(c.y[1] as i16),
            V::splat16(c.y[2] as i16),
        ];
        let round = V::splat16(128);
        let off = V::splat16(c.y_offset as i16);
        while x + n <= width {
            let px = [
                V::load(planes[0].as_ptr().add(x)),
                V::load(planes[1].as_ptr().add(x)),
                V::load(planes[2].as_ptr().add(x)),
            ];
            let lo = [
                V::unpacklo8(px[0], z),
                V::unpacklo8(px[1], z),
                V::unpacklo8(px[2], z),
            ];
            let hi = [
                V::unpackhi8(px[0], z),
                V::unpackhi8(px[1], z),
                V::unpackhi8(px[2], z),
            ];
            let lo = dot3(lo, &k, round, off, false);
            let hi = dot3(hi, &k, round, off, false);
            V::store(dst.as_mut_ptr().add(x), V::packus16(lo, hi));
            x += n;
        }
    }
    x
}

// Many borrowed rows; grouping them would only rename the scalar oracle's arguments.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(super) unsafe fn rgb_to_uv<V: Vx>(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) -> usize {
    let n = V::BYTES / 2;
    let mut i = 0;
    // SAFETY: `2n` bytes of each input row read at `2i`; `2n` (interleaved) or `n` bytes written
    // at `2i` / `i`, with `i + n <= width`.
    unsafe {
        let low = V::splat16(0xFF);
        let two = V::splat16(2);
        let ku = [V::splat16(c.u[0]), V::splat16(c.u[1]), V::splat16(c.u[2])];
        let kv = [V::splat16(c.v[0]), V::splat16(c.v[1]), V::splat16(c.v[2])];
        let (r64, c128) = (V::splat16(64), V::splat16(128));
        while i + n <= width {
            let at = |p: &[u8]| p.as_ptr().add(2 * i);
            let mean = [
                mean4(at(top[0]), at(bottom[0]), low, two),
                mean4(at(top[1]), at(bottom[1]), low, two),
                mean4(at(top[2]), at(bottom[2]), low, two),
            ];
            let cu = dot3(mean, &ku, r64, c128, true);
            let cv = dot3(mean, &kv, r64, c128, true);
            let (cu, cv) = (V::packus16(cu, cu), V::packus16(cv, cv));
            if interleaved {
                V::store(u.as_mut_ptr().add(2 * i), V::unpacklo8(cu, cv));
            } else {
                V::store_half(u.as_mut_ptr().add(i), V::fix_pack(cu));
                V::store_half(v.as_mut_ptr().add(i), V::fix_pack(cv));
            }
            i += n;
        }
    }
    i
}

macro_rules! instantiate {
    ($($body:ident => $sse:ident, $avx:ident ($($arg:ident: $ty:ty),*);)*) => {$(
        /// # Safety
        /// SSE2 must be available; slices sized as the dispatcher does.
        #[allow(clippy::too_many_arguments)]
        #[target_feature(enable = "sse2")]
        pub(in crate::simd) unsafe fn $sse($($arg: $ty),*) -> usize {
            // SAFETY: forwarded from the caller.
            unsafe { $body::<__m128i>($($arg),*) }
        }
        /// # Safety
        /// AVX2 must be available; slices sized as the dispatcher does.
        #[allow(clippy::too_many_arguments)]
        #[target_feature(enable = "avx2")]
        pub(in crate::simd) unsafe fn $avx($($arg: $ty),*) -> usize {
            // SAFETY: forwarded from the caller.
            unsafe { $body::<__m256i>($($arg),*) }
        }
    )*};
}

instantiate! {
    ccm => ccm_sse2, ccm_avx2(planes: [&mut [u16]; 3], m: &[i16; 9], width: usize);
    narrow => narrow_sse2, narrow_avx2(src: &[u16], dst: &mut [u8], width: usize);
    rgb_to_y => rgb_to_y_sse2, rgb_to_y_avx2(
        planes: [&[u8]; 3], dst: &mut [u8], width: usize, c: &YuvCoeffs);
    rgb_to_uv => rgb_to_uv_sse2, rgb_to_uv_avx2(
        top: [&[u8]; 3], bottom: [&[u8]; 3], u: &mut [u8], v: &mut [u8], width: usize,
        c: &YuvCoeffs, interleaved: bool);
}

/// Shuffle masks taking channel `ch` of 16 planar pixels into output block `block` (0..3) of
/// packed RGB24.
const fn interleave_mask(block: usize, ch: usize) -> [u8; 16] {
    let mut m = [0x80u8; 16];
    let mut j = 0;
    while j < 16 {
        let n = block * 16 + j;
        if n % 3 == ch {
            m[j] = (n / 3) as u8;
        }
        j += 1;
    }
    m
}

const INTERLEAVE: [[[u8; 16]; 3]; 3] = {
    let mut t = [[[0u8; 16]; 3]; 3];
    let mut block = 0;
    while block < 3 {
        let mut ch = 0;
        while ch < 3 {
            t[block][ch] = interleave_mask(block, ch);
            ch += 1;
        }
        block += 1;
    }
    t
};

#[inline(always)]
unsafe fn mask(m: &[u8; 16]) -> __m128i {
    // SAFETY: an unaligned 16-byte load of a constant.
    unsafe { _mm_loadu_si128(m.as_ptr().cast()) }
}

/// # Safety
/// SSSE3 must be available; planes hold `width` bytes, `dst` `3 * width`.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn interleave_rgb_ssse3(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
) -> usize {
    let mut x = 0;
    // SAFETY: 16 bytes of each plane at `x` and 48 output bytes at `3x`, `x + 16 <= width`.
    unsafe {
        while x + 16 <= width {
            let px = [
                _mm_loadu_si128(planes[0].as_ptr().add(x).cast()),
                _mm_loadu_si128(planes[1].as_ptr().add(x).cast()),
                _mm_loadu_si128(planes[2].as_ptr().add(x).cast()),
            ];
            for (block, masks) in INTERLEAVE.iter().enumerate() {
                let v = _mm_or_si128(
                    _mm_or_si128(
                        _mm_shuffle_epi8(px[0], mask(&masks[0])),
                        _mm_shuffle_epi8(px[1], mask(&masks[1])),
                    ),
                    _mm_shuffle_epi8(px[2], mask(&masks[2])),
                );
                _mm_storeu_si128(dst.as_mut_ptr().add(3 * x + 16 * block).cast(), v);
            }
            x += 16;
        }
    }
    x
}

/// RAW10: byte indexes of each pixel's high bits and of its group's low-bits byte, as 16-bit
/// lanes; the low bits are then isolated by multiplying by `4^(3 - lane)` and shifting.
const RAW10_HIGH: [u8; 16] = [
    0, 0x80, 1, 0x80, 2, 0x80, 3, 0x80, 5, 0x80, 6, 0x80, 7, 0x80, 8, 0x80,
];
const RAW10_LOW: [u8; 16] = [
    4, 0x80, 4, 0x80, 4, 0x80, 4, 0x80, 9, 0x80, 9, 0x80, 9, 0x80, 9, 0x80,
];
const RAW12_HIGH: [u8; 16] = [
    0, 0x80, 1, 0x80, 3, 0x80, 4, 0x80, 6, 0x80, 7, 0x80, 9, 0x80, 10, 0x80,
];
const RAW12_LOW: [u8; 16] = [
    2, 0x80, 2, 0x80, 5, 0x80, 5, 0x80, 8, 0x80, 8, 0x80, 11, 0x80, 11, 0x80,
];

/// Eight pixels from one 16-byte load (bytes `[0, 10)` or `[0, 12)` used), per 128-bit half.
#[inline(always)]
unsafe fn unpack8<V: Vx>(v: V, raw12: bool) -> V {
    unsafe {
        if raw12 {
            let high = V::slli16::<4>(V::shuffle_bytes(v, &RAW12_HIGH));
            let low = V::srli16::<4>(V::mullo16(V::shuffle_bytes(v, &RAW12_LOW), pair(16, 1)));
            V::or(high, V::and(low, V::splat16(0xF)))
        } else {
            let high = V::slli16::<2>(V::shuffle_bytes(v, &RAW10_HIGH));
            let low = V::mullo16(
                V::shuffle_bytes(v, &RAW10_LOW),
                V::splat64(64 | 16 << 16 | 4 << 32 | 1 << 48),
            );
            V::or(high, V::and(V::srli16::<6>(low), V::splat16(3)))
        }
    }
}

/// # Safety
/// SSSE3 must be available; `src` holds the packed row, `dst` `width` samples.
#[target_feature(enable = "ssse3")]
pub(in crate::simd) unsafe fn unpack_ssse3(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
    raw12: bool,
) -> usize {
    let step = if raw12 { 12 } else { 10 };
    let (mut x, mut off) = (0, 0);
    // SAFETY: 16 bytes read at `off` and 8 samples written at `x`, both checked.
    unsafe {
        while x + 8 <= width && off + 16 <= src.len() {
            let v = unpack8::<__m128i>(_mm_loadu_si128(src.as_ptr().add(off).cast()), raw12);
            _mm_storeu_si128(dst.as_mut_ptr().add(x).cast(), v);
            x += 8;
            off += step;
        }
    }
    x
}

/// # Safety
/// AVX2 must be available; as [`unpack_ssse3`].
#[target_feature(enable = "avx2")]
pub(in crate::simd) unsafe fn unpack_avx2(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
    raw12: bool,
) -> usize {
    let step = if raw12 { 12 } else { 10 };
    let (mut x, mut off) = (0, 0);
    // SAFETY: 16 bytes read at `off` and at `off + step`, 16 samples written at `x`.
    unsafe {
        while x + 16 <= width && off + step + 16 <= src.len() {
            let p = src.as_ptr().add(off);
            let v = _mm256_inserti128_si256::<1>(
                _mm256_castsi128_si256(_mm_loadu_si128(p.cast())),
                _mm_loadu_si128(p.add(step).cast()),
            );
            _mm256_storeu_si256(dst.as_mut_ptr().add(x).cast(), unpack8::<__m256i>(v, raw12));
            x += 16;
            off += 2 * step;
        }
    }
    x
}
