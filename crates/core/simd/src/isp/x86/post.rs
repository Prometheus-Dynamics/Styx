//! x86 leaves of the output picture's stages, one body per kernel over [`Vx`]. Sums stay in 16
//! bits; the products that do not (a Q8 scale of a difference) go through `pmaddwd`.

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

use super::vec::Vx;
use super::{SimdBackend, X86FeatureSet, done};
use crate::isp::post::RCP;

/// Shuffle mask taking channel `ch` of 16 packed RGB24 pixels from input block `block` (0 to 2)
/// to its place among the 16 samples of that channel.
const fn deinterleave_mask(block: usize, ch: usize) -> [u8; 16] {
    let mut m = [0x80u8; 16];
    let mut i = 0;
    while i < 16 {
        let at = 3 * i + ch;
        if at / 16 == block {
            m[i] = (at % 16) as u8;
        }
        i += 1;
    }
    m
}

const DEINTERLEAVE: [[[u8; 16]; 3]; 3] = {
    let mut t = [[[0u8; 16]; 3]; 3];
    let mut block = 0;
    while block < 3 {
        let mut ch = 0;
        while ch < 3 {
            t[block][ch] = deinterleave_mask(block, ch);
            ch += 1;
        }
        block += 1;
    }
    t
};

/// The low and high bytes of [`RCP`], as lookup tables.
const fn rcp_bytes(shift: u32) -> [u8; 16] {
    let mut t = [0u8; 16];
    let mut i = 0;
    while i < RCP.len() {
        t[i] = (RCP[i] >> shift) as u8;
        i += 1;
    }
    t
}
const RCP_LO: [u8; 16] = rcp_bytes(0);
const RCP_HI: [u8; 16] = rcp_bytes(8);

/// `(d k + 128) >> 8` of signed 16-bit `d`, with `k` from [`q8_factor`]; the result must fit.
#[inline(always)]
unsafe fn scale_q8<V: Vx>(d: V, factor: V) -> V {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe {
        let one = V::splat16(1);
        let lo = V::madd16(V::unpacklo16(d, one), factor);
        let hi = V::madd16(V::unpackhi16(d, one), factor);
        V::packs32(V::srai32::<8>(lo), V::srai32::<8>(hi))
    }
}

/// The `pmaddwd` factor of [`scale_q8`]: `k` times the value plus 128 times the 1 beside it.
#[inline(always)]
unsafe fn q8_factor<V: Vx>(k: i16) -> V {
    // SAFETY: a splat; the caller enabled the feature.
    unsafe { V::splat32((128 << 16) | i32::from(k as u16)) }
}

/// `|a - b|` of lanes in `0 ..= 32767`.
#[inline(always)]
unsafe fn abs_diff<V: Vx>(a: V, b: V) -> V {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe { V::sub16(V::max_i16(a, b), V::min_i16(a, b)) }
}

/// Three registers of 16 samples per 128-bit half from 48 bytes per half of packed RGB24.
#[inline(always)]
unsafe fn load_rgb<V: Vx>(p: *const u8) -> [V; 3] {
    // SAFETY: the caller guarantees `3 * V::BYTES` readable bytes at `p`; AVX2's upper halves
    // are the second 48 of them.
    unsafe {
        let blocks = [
            V::load_halves(p, p.add(48)),
            V::load_halves(p.add(16), p.add(64)),
            V::load_halves(p.add(32), p.add(80)),
        ];
        let mut out = [V::zero(); 3];
        for (ch, o) in out.iter_mut().enumerate() {
            *o = V::or(
                V::or(
                    V::shuffle_bytes(blocks[0], &DEINTERLEAVE[0][ch]),
                    V::shuffle_bytes(blocks[1], &DEINTERLEAVE[1][ch]),
                ),
                V::shuffle_bytes(blocks[2], &DEINTERLEAVE[2][ch]),
            );
        }
        out
    }
}

/// `((k R + k G + k B + 128) >> 8) + 128` in signed 16 bits. The sum reaches 32640 and only
/// the rounding can pass `i16`: saturating it gives 127 + 128 = 255, which the clamp would.
#[inline(always)]
unsafe fn chroma_lane<V: Vx>(px: [V; 3], k: [i16; 3]) -> V {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe {
        let s = V::add16(
            V::add16(
                V::mullo16(px[0], V::splat16(k[0])),
                V::mullo16(px[1], V::splat16(k[1])),
            ),
            V::mullo16(px[2], V::splat16(k[2])),
        );
        let c = V::splat16(128);
        V::add16(V::srai16::<8>(V::adds_i16(s, c)), c)
    }
}

#[inline(always)]
unsafe fn rgb_to_planes<V: Vx>(
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> usize {
    let n = V::BYTES;
    let mut x = 0;
    // SAFETY: `3 n` bytes read and `n` written per plane at `x`, `x + n <= width`.
    unsafe {
        let z = V::zero();
        while x + n <= width {
            let px = load_rgb::<V>(rgb.as_ptr().add(3 * x));
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
            let luma = |l: [V; 3]| {
                let s = V::add16(
                    V::add16(
                        V::mullo16(l[0], V::splat16(77)),
                        V::mullo16(l[1], V::splat16(150)),
                    ),
                    V::add16(V::mullo16(l[2], V::splat16(29)), V::splat16(128)),
                );
                V::srli16::<8>(s)
            };
            V::store(y.as_mut_ptr().add(x), V::packus16(luma(lo), luma(hi)));
            V::store(
                cb.as_mut_ptr().add(x),
                V::packus16(
                    chroma_lane(lo, [-43, -85, 128]),
                    chroma_lane(hi, [-43, -85, 128]),
                ),
            );
            V::store(
                cr.as_mut_ptr().add(x),
                V::packus16(
                    chroma_lane(lo, [128, -107, -21]),
                    chroma_lane(hi, [128, -107, -21]),
                ),
            );
            x += n;
        }
    }
    x
}

/// R, G, B of 16-bit `y`, `cb - 128`, `cr - 128`. Each large coefficient is split so the shifted
/// part stays in 16 bits: `359 = 256 + 103`, `-183 = -256 + 73`, `454 = 512 - 58`.
#[inline(always)]
unsafe fn rgb_lanes<V: Vx>(y: V, cb: V, cr: V) -> [V; 3] {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe {
        let round = V::splat16(128);
        let r = V::srai16::<8>(V::add16(V::mullo16(cr, V::splat16(103)), round));
        let g = V::srai16::<8>(V::add16(
            V::add16(
                V::mullo16(cb, V::splat16(-88)),
                V::mullo16(cr, V::splat16(73)),
            ),
            round,
        ));
        let b = V::srai16::<8>(V::add16(V::mullo16(cb, V::splat16(-58)), round));
        [
            V::add16(V::add16(y, cr), r),
            V::add16(V::sub16(y, cr), g),
            V::add16(V::add16(y, V::add16(cb, cb)), b),
        ]
    }
}

#[inline(always)]
unsafe fn planes_to_rgb<V: Vx>(planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize) -> usize {
    let n = V::BYTES;
    let [r, g, b] = rgb;
    let mut x = 0;
    // SAFETY: `n` bytes of each plane read and written at `x`, `x + n <= width`.
    unsafe {
        let z = V::zero();
        let mid = V::splat16(128);
        while x + n <= width {
            let px = planes.map(|p| V::load(p.as_ptr().add(x)));
            let lo = rgb_lanes(
                V::unpacklo8(px[0], z),
                V::sub16(V::unpacklo8(px[1], z), mid),
                V::sub16(V::unpacklo8(px[2], z), mid),
            );
            let hi = rgb_lanes(
                V::unpackhi8(px[0], z),
                V::sub16(V::unpackhi8(px[1], z), mid),
                V::sub16(V::unpackhi8(px[2], z), mid),
            );
            V::store(r.as_mut_ptr().add(x), V::packus16(lo[0], hi[0]));
            V::store(g.as_mut_ptr().add(x), V::packus16(lo[1], hi[1]));
            V::store(b.as_mut_ptr().add(x), V::packus16(lo[2], hi[2]));
            x += n;
        }
    }
    x
}

/// One half of the sigma filter's vector: the centres `c`, thresholds `t` and neighbours in 16
/// bits to the filtered samples. The sum stays at most `9 * 255`; the mean is `mulhi` plus the
/// carry of the rounding out of `mullo`.
#[inline(always)]
unsafe fn sigma_lanes<V: Vx>(c: V, t: V, n: [V; 8], factor: V) -> V {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe {
        let (mut sum, mut cnt) = (c, V::splat16(1));
        // The threshold plus 1: `<=` as `<`.
        let t = V::add16(t, V::splat16(1));
        for v in n {
            let ok = V::cmplt_i16(V::slli16::<2>(abs_diff(v, c)), t);
            sum = V::add16(sum, V::and(ok, v));
            cnt = V::sub16(cnt, ok);
        }
        // The high byte of a count looks up entry 0, which is 0 in both tables.
        let rcp = V::or(
            V::lookup_bytes(&RCP_LO, cnt),
            V::slli16::<8>(V::lookup_bytes(&RCP_HI, cnt)),
        );
        let avg = V::add16(
            V::mulhi_u16(sum, rcp),
            V::srli16::<15>(V::mullo16(sum, rcp)),
        );
        V::add16(c, scale_q8(V::sub16(avg, c), factor))
    }
}

#[inline(always)]
unsafe fn sigma<V: Vx>(rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16) -> usize {
    let n = V::BYTES;
    let width = dst.len();
    let [up, cur, dn] = rows;
    let mut x = 0;
    // SAFETY: `n` bytes of each row at `x` to `x + 2`, `x + n <= width`, and each row holds
    // `width + 2`; `n` written at `x`. `lanes` holds `n / 2` samples per half pair at most 32.
    unsafe {
        let z = V::zero();
        let factor = q8_factor::<V>(s);
        let mut lanes = [[0u16; 16]; 2];
        while x + n <= width {
            // In the order of the unpacks: bytes 0-7 of each half low, 8-15 high.
            for i in 0..n {
                let (half, sub) = (i / 16, i % 16);
                lanes[sub / 8][half * 8 + sub % 8] =
                    table[usize::from(*cur.get_unchecked(x + 1 + i))];
            }
            let load = |row: &[u8], at: usize| V::load(row.as_ptr().add(x + at));
            let centre = load(cur, 1);
            let px = [
                load(up, 0),
                load(up, 1),
                load(up, 2),
                load(cur, 0),
                load(cur, 2),
                load(dn, 0),
                load(dn, 1),
                load(dn, 2),
            ];
            let lo = sigma_lanes(
                V::unpacklo8(centre, z),
                V::load(lanes[0].as_ptr().cast()),
                px.map(|v| V::unpacklo8(v, z)),
                factor,
            );
            let hi = sigma_lanes(
                V::unpackhi8(centre, z),
                V::load(lanes[1].as_ptr().cast()),
                px.map(|v| V::unpackhi8(v, z)),
                factor,
            );
            V::store(dst.as_mut_ptr().add(x), V::packus16(lo, hi));
            x += n;
        }
    }
    x
}

/// The vertical `[1 2 1]` sums of columns `x` to `x + 2` of `rows`, low and high lanes, and the
/// samples of the middle row at `x + 1`.
#[inline(always)]
unsafe fn column_sums<V: Vx>(rows: [&[u8]; 3], x: usize) -> ([V; 3], [V; 3], V) {
    // SAFETY: the caller guarantees `V::BYTES` bytes of each row at `x` to `x + 2`.
    unsafe {
        let z = V::zero();
        let column = |at: usize| rows.map(|r| V::load(r.as_ptr().add(x + at)));
        let cols = [column(0), column(1), column(2)];
        let lo = cols.map(|[a, b, c]| {
            let b = V::unpacklo8(b, z);
            V::add16(
                V::add16(V::unpacklo8(a, z), V::unpacklo8(c, z)),
                V::add16(b, b),
            )
        });
        let hi = cols.map(|[a, b, c]| {
            let b = V::unpackhi8(b, z);
            V::add16(
                V::add16(V::unpackhi8(a, z), V::unpackhi8(c, z)),
                V::add16(b, b),
            )
        });
        (lo, hi, cols[1][1])
    }
}

#[inline(always)]
unsafe fn chroma_lanes<V: Vx>(sums: [V; 3], c: V, t: V, factor: V) -> V {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe {
        let low = V::srli16::<4>(V::add16(
            V::add16(V::add16(sums[0], sums[1]), V::add16(sums[1], sums[2])),
            V::splat16(8),
        ));
        let ok = V::cmplt_i16(V::slli16::<2>(abs_diff(low, c)), t);
        V::add16(c, V::and(ok, scale_q8(V::sub16(low, c), factor)))
    }
}

#[inline(always)]
unsafe fn chroma<V: Vx>(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16) -> usize {
    let n = V::BYTES;
    let width = dst.len();
    let mut x = 0;
    // SAFETY: as in `sigma`.
    unsafe {
        let z = V::zero();
        let factor = q8_factor::<V>(s);
        let t = V::splat16(t + 1);
        while x + n <= width {
            let (lo, hi, centre) = column_sums::<V>(rows, x);
            V::store(
                dst.as_mut_ptr().add(x),
                V::packus16(
                    chroma_lanes(lo, V::unpacklo8(centre, z), t, factor),
                    chroma_lanes(hi, V::unpackhi8(centre, z), t, factor),
                ),
            );
            x += n;
        }
    }
    x
}

#[inline(always)]
unsafe fn sharpen_lanes<V: Vx>(sums: [V; 3], c: V, thr: V, limit: V, factor: V) -> V {
    // SAFETY: lane-wise arithmetic; the caller enabled the feature.
    unsafe {
        let blur = V::srli16::<4>(V::add16(
            V::add16(V::add16(sums[0], sums[1]), V::add16(sums[1], sums[2])),
            V::splat16(8),
        ));
        let detail = V::sub16(c, blur);
        let z = V::zero();
        let neg = V::cmplt_i16(detail, z);
        let size = V::max_i16(detail, V::sub16(z, detail));
        let cored = V::max_i16(V::sub16(size, thr), z);
        let signed = V::select(neg, V::sub16(z, cored), cored);
        let corr = scale_q8(signed, factor);
        let corr = V::min_i16(V::max_i16(corr, V::sub16(z, limit)), limit);
        V::add16(c, corr)
    }
}

#[inline(always)]
unsafe fn sharpen<V: Vx>(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    amount: i16,
    thr: i16,
    limit: i16,
) -> usize {
    let n = V::BYTES;
    let width = dst.len();
    let mut x = 0;
    // SAFETY: as in `sigma`.
    unsafe {
        let z = V::zero();
        let factor = q8_factor::<V>(amount);
        let (thr, limit) = (V::splat16(thr), V::splat16(limit));
        while x + n <= width {
            let (lo, hi, centre) = column_sums::<V>(rows, x);
            V::store(
                dst.as_mut_ptr().add(x),
                V::packus16(
                    sharpen_lanes(lo, V::unpacklo8(centre, z), thr, limit, factor),
                    sharpen_lanes(hi, V::unpackhi8(centre, z), thr, limit, factor),
                ),
            );
            x += n;
        }
    }
    x
}

macro_rules! instantiate {
    ($($body:ident, $feature:literal, $name:ident, $v:ident ($($arg:ident: $ty:ty),*);)*) => {$(
        /// # Safety
        /// The feature must be available; slices sized as the dispatcher does.
        #[target_feature(enable = $feature)]
        unsafe fn $name($($arg: $ty),*) -> usize {
            // SAFETY: forwarded from the caller.
            unsafe { $body::<$v>($($arg),*) }
        }
    )*};
}

instantiate! {
    rgb_to_planes, "ssse3", rgb_to_planes_ssse3, __m128i(
        rgb: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8], width: usize);
    rgb_to_planes, "avx2", rgb_to_planes_avx2, __m256i(
        rgb: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8], width: usize);
    planes_to_rgb, "sse2", planes_to_rgb_sse2, __m128i(
        planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize);
    planes_to_rgb, "avx2", planes_to_rgb_avx2, __m256i(
        planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize);
    sigma, "ssse3", sigma_ssse3, __m128i(
        rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16);
    sigma, "avx2", sigma_avx2, __m256i(
        rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16);
    chroma, "sse2", chroma_sse2, __m128i(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16);
    chroma, "avx2", chroma_avx2, __m256i(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16);
    sharpen, "sse2", sharpen_sse2, __m128i(
        rows: [&[u8]; 3], dst: &mut [u8], amount: i16, thr: i16, limit: i16);
    sharpen, "avx2", sharpen_avx2, __m256i(
        rows: [&[u8]; 3], dst: &mut [u8], amount: i16, thr: i16, limit: i16);
}

/// AVX2 from 32 samples, else `$sse` when `$has` holds.
macro_rules! choose {
    ($f:expr, $width:expr, $has:expr, $backend:expr, $avx:expr, $sse:expr) => {{
        // SAFETY (every leaf): the feature it needs was detected; the dispatcher sized the
        // slices for `width` samples.
        if $f.avx2 && $width >= 32 {
            // SAFETY: the feature this leaf needs was detected by the dispatcher, and the rows are sized for the width.
            done(SimdBackend::X86Avx2, unsafe { $avx })
        } else if $has {
            // SAFETY: the feature this leaf needs was detected by the dispatcher, and the rows are sized for the width.
            done($backend, unsafe { $sse })
        } else {
            None
        }
    }};
}

pub(in crate::isp) fn rgb_to_planes_row(
    f: X86FeatureSet,
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    choose!(
        f,
        width,
        f.ssse3,
        SimdBackend::X86Ssse3,
        rgb_to_planes_avx2(rgb, y, cb, cr, width),
        rgb_to_planes_ssse3(rgb, y, cb, cr, width)
    )
}

pub(in crate::isp) fn planes_to_rgb_row(
    f: X86FeatureSet,
    planes: [&[u8]; 3],
    rgb: [&mut [u8]; 3],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    choose!(
        f,
        width,
        f.sse2,
        SimdBackend::X86Sse2,
        planes_to_rgb_avx2(planes, rgb, width),
        planes_to_rgb_sse2(planes, rgb, width)
    )
}

pub(in crate::isp) fn sigma_row(
    f: X86FeatureSet,
    rows: [&[u8]; 3],
    dst: &mut [u8],
    table: &[u16; 256],
    s: i16,
) -> Option<(SimdBackend, usize)> {
    let width = dst.len();
    choose!(
        f,
        width,
        f.ssse3,
        SimdBackend::X86Ssse3,
        sigma_avx2(rows, dst, table, s),
        sigma_ssse3(rows, dst, table, s)
    )
}

pub(in crate::isp) fn chroma_row(
    f: X86FeatureSet,
    rows: [&[u8]; 3],
    dst: &mut [u8],
    t: i16,
    s: i16,
) -> Option<(SimdBackend, usize)> {
    let width = dst.len();
    choose!(
        f,
        width,
        f.sse2,
        SimdBackend::X86Sse2,
        chroma_avx2(rows, dst, t, s),
        chroma_sse2(rows, dst, t, s)
    )
}

pub(in crate::isp) fn sharpen_row(
    f: X86FeatureSet,
    rows: [&[u8]; 3],
    dst: &mut [u8],
    amount: i16,
    thr: i16,
    limit: i16,
) -> Option<(SimdBackend, usize)> {
    let width = dst.len();
    choose!(
        f,
        width,
        f.sse2,
        SimdBackend::X86Sse2,
        sharpen_avx2(rows, dst, amount, thr, limit),
        sharpen_sse2(rows, dst, amount, thr, limit)
    )
}
