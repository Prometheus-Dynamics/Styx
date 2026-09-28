//! Mosaic-domain kernel bodies (front end, demosaic, luma, quads), written once over [`Vx`] and
//! instantiated for SSE2 and AVX2. Each returns the pixels it wrote, a multiple of the lane
//! count, from the start of the row; slices are sized by the dispatcher.

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use super::vec::Vx;
use crate::format::CfaPattern;
use crate::simd::RowKind;

/// u16 lanes per vector.
const fn lanes<V: Vx>() -> usize {
    V::BYTES / 2
}

#[inline(always)]
unsafe fn ld<V: Vx>(p: &[u16], i: usize) -> V {
    // SAFETY: the callers keep `i + lanes` within `p`.
    unsafe { V::load(p.as_ptr().add(i).cast()) }
}

#[inline(always)]
unsafe fn st<V: Vx>(p: &mut [u16], i: usize, v: V) {
    // SAFETY: the callers keep `i + lanes` within `p`.
    unsafe { V::store(p.as_mut_ptr().add(i).cast(), v) }
}

/// A 16-bit pair `(lo, hi)` broadcast to every 32-bit lane.
#[inline(always)]
unsafe fn pair<V: Vx>(lo: i16, hi: i16) -> V {
    unsafe { V::splat32((lo as u16 as u32 | (hi as u16 as u32) << 16) as i32) }
}

/// All-ones in the lanes of green columns (lane parity equals column parity: x starts even).
#[inline(always)]
unsafe fn green_mask<V: Vx>(kind: RowKind) -> V {
    unsafe {
        if kind.green_even {
            pair(-1, 0)
        } else {
            pair(0, -1)
        }
    }
}

#[inline(always)]
pub(super) unsafe fn front<V: Vx>(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> usize {
    let n = lanes::<V>();
    let mut x = 0;
    // SAFETY: loads and stores of `n` lanes at `x`, with `x + n <= width`.
    unsafe {
        let bl = pair::<V>(black[0] as i16, black[1] as i16);
        let count = _mm_cvtsi32_si128(shift as i32);
        while x + n <= width {
            let v = V::sll16(V::subs_u16(ld(row, x), bl), count);
            let v = V::min_u16(V::mulhi_u16(v, ld(gains, x)), 4095);
            st(row, x, v);
            x += n;
        }
    }
    x
}

#[inline(always)]
unsafe fn store_rgb<V: Vx>(out: &mut [&mut [u16]; 3], x: usize, kind: RowKind, g: V, xv: V, yv: V) {
    let (r, b) = if kind.x_is_red { (xv, yv) } else { (yv, xv) };
    // SAFETY: the callers keep `x + lanes` within the output rows.
    unsafe {
        st(out[0], x, r);
        st(out[1], x, g);
        st(out[2], x, b);
    }
}

#[inline(always)]
pub(super) unsafe fn demosaic_bilinear<V: Vx>(
    rows: [&[u16]; 3],
    mut out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> usize {
    let [up, cur, dn] = rows;
    let n = lanes::<V>();
    let mut x = 0;
    // SAFETY: input rows hold `width + 2` samples, so loads at `x + 2` of `n` lanes stay inside
    // while `x + n <= width`; outputs hold `width`.
    unsafe {
        let mask = green_mask::<V>(kind);
        let two = V::splat16(2);
        while x + n <= width {
            let (l, c, r) = (ld::<V>(cur, x), ld::<V>(cur, x + 1), ld::<V>(cur, x + 2));
            let (u, d) = (ld::<V>(up, x + 1), ld::<V>(dn, x + 1));
            let h = V::add16(l, r);
            let v = V::add16(u, d);
            let diag = V::add16(
                V::add16(ld(up, x), ld(up, x + 2)),
                V::add16(ld(dn, x), ld(dn, x + 2)),
            );
            let g_cross = V::srli16::<2>(V::add16(V::add16(h, v), two));
            let y_diag = V::srli16::<2>(V::add16(diag, two));
            let g = V::select(mask, c, g_cross);
            let xv = V::select(mask, V::avg_u16(l, r), c);
            let yv = V::select(mask, V::avg_u16(u, d), y_diag);
            store_rgb(&mut out, x, kind, g, xv, yv);
            x += n;
        }
    }
    x
}

/// `clamp((a0 wa0 + a1 wa1 + b0 wb0 + b1 wb1 + 8) >> 4, 0, 4095)` per 16-bit lane, in 32 bits.
#[inline(always)]
unsafe fn mix<V: Vx>(a: (V, V), wa: V, b: (V, V), wb: V) -> V {
    unsafe {
        let eight = V::splat32(8);
        let lo = V::add32(
            V::add32(
                V::madd16(V::unpacklo16(a.0, a.1), wa),
                V::madd16(V::unpacklo16(b.0, b.1), wb),
            ),
            eight,
        );
        let hi = V::add32(
            V::add32(
                V::madd16(V::unpackhi16(a.0, a.1), wa),
                V::madd16(V::unpackhi16(b.0, b.1), wb),
            ),
            eight,
        );
        let v = V::packs32(V::srai32::<4>(lo), V::srai32::<4>(hi));
        V::min_i16(V::max_i16(v, V::zero()), V::splat16(4095))
    }
}

#[inline(always)]
pub(super) unsafe fn demosaic_mhc<V: Vx>(
    rows: [&[u16]; 5],
    mut out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> usize {
    let [u2, u1, c0, d1, d2] = rows;
    let n = lanes::<V>();
    let mut x = 0;
    // SAFETY: input rows hold `width + 4` samples, so loads at `x + 4` of `n` lanes stay inside
    // while `x + n <= width`; outputs hold `width`.
    unsafe {
        let mask = green_mask::<V>(kind);
        let zero = V::zero();
        while x + n <= width {
            let c = ld::<V>(c0, x + 2);
            let far_h = V::add16(ld(c0, x), ld(c0, x + 4));
            let far_v = V::add16(ld(u2, x + 2), ld(d2, x + 2));
            let near_h = V::add16(ld(c0, x + 1), ld(c0, x + 3));
            let near_v = V::add16(ld(u1, x + 2), ld(d1, x + 2));
            let diag = V::add16(
                V::add16(ld(u1, x + 1), ld(u1, x + 3)),
                V::add16(ld(d1, x + 1), ld(d1, x + 3)),
            );
            // Green sites: X along the row, Y across it.
            let xs = mix(
                (c, near_h),
                pair(10, 8),
                (V::add16(far_h, diag), far_v),
                pair(-2, 1),
            );
            let ys = mix(
                (c, near_v),
                pair(10, 8),
                (V::add16(far_v, diag), far_h),
                pair(-2, 1),
            );
            // Red / blue sites: green, and the opposite colour from the diagonals.
            let far = V::add16(far_h, far_v);
            let gs = mix(
                (c, V::add16(near_h, near_v)),
                pair(8, 4),
                (far, zero),
                pair(-2, 0),
            );
            let y2 = mix((c, diag), pair(12, 4), (far, zero), pair(-3, 0));
            let g = V::select(mask, c, gs);
            let xv = V::select(mask, xs, c);
            let yv = V::select(mask, ys, y2);
            store_rgb(&mut out, x, kind, g, xv, yv);
            x += n;
        }
    }
    x
}

/// `up + 2 cur + dn` at `i`.
#[inline(always)]
unsafe fn vsum<V: Vx>([up, cur, dn]: [&[u16]; 3], i: usize) -> V {
    // SAFETY: the callers keep `i + lanes` within the rows.
    unsafe {
        V::add16(
            V::add16(ld::<V>(up, i), ld::<V>(dn, i)),
            V::slli16::<1>(ld::<V>(cur, i)),
        )
    }
}

/// Rounded quarter of the quad sums of `lanes` samples of `top` and `bottom` at `j`, in 32 bits.
#[inline(always)]
unsafe fn quad_sums<V: Vx>(top: &[u16], bottom: &[u16], j: usize) -> V {
    // SAFETY: the callers keep `j + lanes` within both rows.
    unsafe {
        let ones = V::splat16(1);
        let s = V::add32(
            V::madd16(ld::<V>(top, j), ones),
            V::madd16(ld::<V>(bottom, j), ones),
        );
        V::srai32::<2>(V::add32(s, V::splat32(2)))
    }
}

#[inline(always)]
pub(super) unsafe fn bayer_luma<V: Vx>(rows: [&[u16]; 3], dst: &mut [u16], width: usize) -> usize {
    let n = lanes::<V>();
    let mut x = 0;
    // SAFETY: as `demosaic_bilinear`.
    unsafe {
        let eight = V::splat16(8);
        while x + n <= width {
            let (a, b, c) = (
                vsum::<V>(rows, x),
                vsum::<V>(rows, x + 1),
                vsum::<V>(rows, x + 2),
            );
            let s = V::add16(V::add16(a, c), V::slli16::<1>(b));
            st(dst, x, V::srli16::<4>(V::add16(s, eight)));
            x += n;
        }
    }
    x
}

/// Even and odd samples of `2 * lanes` samples at `i`, each in sample order.
#[inline(always)]
unsafe fn deinterleave<V: Vx>(p: &[u16], i: usize) -> (V, V) {
    // SAFETY: the callers keep `i + 2 * lanes` within `p`; samples are 12-bit, so the signed
    // packs never saturate.
    unsafe {
        let (a, b) = (ld::<V>(p, i), ld::<V>(p, i + lanes::<V>()));
        let low = pair::<V>(-1, 0);
        let even = V::fix_pack(V::packs32(V::and(a, low), V::and(b, low)));
        let odd = V::fix_pack(V::packs32(V::srai32::<16>(a), V::srai32::<16>(b)));
        (even, odd)
    }
}

#[inline(always)]
pub(super) unsafe fn quad_rgb<V: Vx>(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> usize {
    let [r, g, b] = out;
    let n = lanes::<V>();
    let mut i = 0;
    // SAFETY: inputs hold `2 * width` samples, outputs `width`; `i + n <= width`.
    unsafe {
        while i + n <= width {
            let (te, to) = deinterleave::<V>(top, 2 * i);
            let (be, bo) = deinterleave::<V>(bottom, 2 * i);
            let (rv, gv, bv) = match pattern {
                CfaPattern::Rggb => (te, V::avg_u16(to, be), bo),
                CfaPattern::Bggr => (bo, V::avg_u16(to, be), te),
                CfaPattern::Grbg => (to, V::avg_u16(te, bo), be),
                CfaPattern::Gbrg => (be, V::avg_u16(te, bo), to),
            };
            st(r, i, rv);
            st(g, i, gv);
            st(b, i, bv);
            i += n;
        }
    }
    i
}

#[inline(always)]
pub(super) unsafe fn quad_luma<V: Vx>(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u16],
    width: usize,
) -> usize {
    let n = lanes::<V>();
    let mut i = 0;
    // SAFETY: as `quad_rgb`.
    unsafe {
        while i + n <= width {
            let (a, b) = (
                quad_sums::<V>(top, bottom, 2 * i),
                quad_sums::<V>(top, bottom, 2 * i + n),
            );
            let v = V::fix_pack(V::packs32(a, b));
            st(dst, i, v);
            i += n;
        }
    }
    i
}

macro_rules! instantiate {
    ($($body:ident => $sse:ident, $avx:ident ($($arg:ident: $ty:ty),*);)*) => {$(
        /// # Safety
        /// SSE2 must be available; slices sized as the dispatcher does.
        #[target_feature(enable = "sse2")]
        pub(in crate::simd) unsafe fn $sse($($arg: $ty),*) -> usize {
            // SAFETY: forwarded from the caller.
            unsafe { $body::<__m128i>($($arg),*) }
        }
        /// # Safety
        /// AVX2 must be available; slices sized as the dispatcher does.
        #[target_feature(enable = "avx2")]
        pub(in crate::simd) unsafe fn $avx($($arg: $ty),*) -> usize {
            // SAFETY: forwarded from the caller.
            unsafe { $body::<__m256i>($($arg),*) }
        }
    )*};
}

instantiate! {
    front => front_sse2, front_avx2(
        row: &mut [u16], black: [u16; 2], gains: &[u16], shift: u32, width: usize);
    demosaic_bilinear => demosaic_bilinear_sse2, demosaic_bilinear_avx2(
        rows: [&[u16]; 3], out: [&mut [u16]; 3], width: usize, kind: RowKind);
    demosaic_mhc => demosaic_mhc_sse2, demosaic_mhc_avx2(
        rows: [&[u16]; 5], out: [&mut [u16]; 3], width: usize, kind: RowKind);
    bayer_luma => bayer_luma_sse2, bayer_luma_avx2(
        rows: [&[u16]; 3], dst: &mut [u16], width: usize);
    quad_rgb => quad_rgb_sse2, quad_rgb_avx2(
        top: &[u16], bottom: &[u16], out: [&mut [u16]; 3], width: usize, pattern: CfaPattern);
    quad_luma => quad_luma_sse2, quad_luma_avx2(
        top: &[u16], bottom: &[u16], dst: &mut [u16], width: usize);
}
