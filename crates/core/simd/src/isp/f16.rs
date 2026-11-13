//! IEEE 754 half precision (fp16) arithmetic in software, bit for bit what AArch64's FP16
//! instructions compute with the default rounding (to nearest, ties to even, subnormals kept):
//! each operation is done exactly in `f64` and rounded once. The oracle of the
//! `Arithmetic::Half` kernels, and their fallback without FP16
//! hardware.
//!
//! Exactness: sums and products of two fp16 values are exact in `f64`, so `add`, `sub` and `mul`
//! round once. `fma` rounds the exact product into an f64 sum before the fp16 rounding; for fp16
//! operands that second rounding cannot change the result (the product has 22 significant bits,
//! so the f64 sum is exact unless the terms are far apart, and then the sum cannot land on an
//! fp16 tie). `fma_is_the_exact_fused_result` checks this against an integer reference.

#[cfg(not(feature = "std"))]
use styx_math::Float as _;

/// Positive infinity.
pub const INF: u16 = 0x7C00;

/// The fp16 nearest to `v` (ties to even; overflow to infinity; NaN to a quiet NaN).
pub fn from_f64(v: f64) -> u16 {
    if v.is_nan() {
        return 0x7E00;
    }
    let sign = if v.is_sign_negative() { 0x8000 } else { 0 };
    let a = v.abs();
    if a >= 65520.0 {
        return sign | INF;
    }
    if a < 2f64.powi(-14) {
        // Subnormal: multiples of 2^-24 (a carry into 0x400 is the smallest normal).
        return sign | (a * 2f64.powi(24)).round_ties_even() as u16;
    }
    let exp = ((a.to_bits() >> 52) & 0x7FF) as i32 - 1023;
    let mut e = exp;
    let mut q = ((a / 2f64.powi(exp) - 1.0) * 1024.0).round_ties_even() as u32;
    if q == 1024 {
        q = 0;
        e += 1;
    }
    if e > 15 {
        return sign | INF;
    }
    sign | (((e + 15) as u32) << 10 | q) as u16
}

/// The value of fp16 bits.
pub fn to_f64(h: u16) -> f64 {
    let e = i32::from((h >> 10) & 0x1F);
    let m = f64::from(h & 0x3FF);
    let v = match e {
        0 => m * 2f64.powi(-24),
        31 if m == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        _ => (1.0 + m / 1024.0) * 2f64.powi(e - 15),
    };
    if h & 0x8000 != 0 { -v } else { v }
}

/// The fp16 nearest to `v` (as [`from_f64`], with bit operations: fast enough for tables).
pub fn from_f32(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xFF) as i32;
    let man = x & 0x7F_FFFF;
    if exp == 0xFF {
        return sign | INF | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 31 {
        return sign | INF;
    }
    if e <= 0 {
        // Subnormal (or zero): the 24-bit significand shifted to multiples of 2^-24.
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let rest = m & ((1 << shift) - 1);
        let half = 1 << (shift - 1);
        let mut q = m >> shift;
        if rest > half || (rest == half && q & 1 == 1) {
            q += 1;
        }
        return sign | q as u16;
    }
    let mut r = (e as u32) << 10 | man >> 13;
    let rest = man & 0x1FFF;
    if rest > 0x1000 || (rest == 0x1000 && r & 1 == 1) {
        r += 1;
    }
    sign | r as u16
}

#[inline]
pub fn add(a: u16, b: u16) -> u16 {
    from_f64(to_f64(a) + to_f64(b))
}

#[inline]
pub fn sub(a: u16, b: u16) -> u16 {
    from_f64(to_f64(a) - to_f64(b))
}

#[inline]
pub fn mul(a: u16, b: u16) -> u16 {
    from_f64(to_f64(a) * to_f64(b))
}

/// `a + b c`, rounded once (`fmla`).
#[inline]
pub fn fma(a: u16, b: u16, c: u16) -> u16 {
    from_f64(to_f64(a) + to_f64(b) * to_f64(c))
}

/// The larger (`fmax`: +0 is larger than -0; the ISP never produces NaN).
#[inline]
pub fn max(a: u16, b: u16) -> u16 {
    let (x, y) = (to_f64(a), to_f64(b));
    if x == y {
        a & b
    } else if x > y {
        a
    } else {
        b
    }
}

/// The smaller (`fmin`: -0 is smaller than +0).
#[inline]
pub fn min(a: u16, b: u16) -> u16 {
    let (x, y) = (to_f64(a), to_f64(b));
    if x == y {
        a | b
    } else if x < y {
        a
    } else {
        b
    }
}

/// To an unsigned 16-bit integer, rounded to nearest with ties to even and saturated
/// (`fcvtnu`).
#[inline]
pub fn to_u16_round(a: u16) -> u16 {
    let v = to_f64(a);
    if v.is_nan() {
        return 0;
    }
    v.round_ties_even().clamp(0.0, 65535.0) as u16
}

/// `1024 + v` for `v` in 0..=1023: the bits of a 10-bit integer placed in an fp16 mantissa.
#[inline]
pub const fn biased(v: u16) -> u16 {
    0x6400 | v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rounds_to_even() {
        for h in (0u16..0x7C00).chain(0x8000..0xFC00) {
            assert_eq!(from_f64(to_f64(h)), h, "{h:#06x}");
        }
        assert_eq!(from_f64(1.0), 0x3C00);
        assert_eq!(from_f64(4094.0), 0x6BFF);
        assert_eq!(from_f64(4095.0), 0x6C00);
        // 2049 lies halfway between 2048 and 2050: ties to even (2048).
        assert_eq!(to_f64(from_f64(2049.0)), 2048.0);
        assert_eq!(to_f64(from_f64(2051.0)), 2052.0);
        assert_eq!(from_f64(65519.0), 0x7BFF);
        assert_eq!(from_f64(65520.0), INF);
        assert_eq!(from_f64(2f64.powi(-24)), 1);
        assert_eq!(from_f64(2f64.powi(-25)), 0);
        assert_eq!(to_f64(biased(77)), 1101.0);
        assert_eq!(to_u16_round(from_f64(2.5)), 2);
        assert_eq!(to_u16_round(from_f64(-3.0)), 0);
        assert_eq!(
            fma(from_f64(1.0), from_f64(3.0), from_f64(0.5)),
            from_f64(2.5)
        );
    }

    /// `a + b c` for finite, non-zero fp16 `a`, `b`, `c`, rounded once from the exact integer
    /// sum (no intermediate rounding at all): the reference [`fma`] is checked against.
    fn exact_fma(a: u16, b: u16, c: u16) -> u16 {
        // (sign, mantissa, exponent): the value is (-1)^sign * mantissa * 2^exponent.
        fn parts(h: u16) -> (bool, i128, i32) {
            let sign = h & 0x8000 != 0;
            let e = i32::from((h >> 10) & 0x1F);
            let m = i128::from(h & 0x3FF);
            if e == 0 {
                (sign, m, -24)
            } else {
                (sign, m | 0x400, e - 25)
            }
        }
        let (sa, ma, ea) = parts(a);
        let (sb, mb, eb) = parts(b);
        let (sc, mc, ec) = parts(c);
        let (sp, mp, ep) = (sb ^ sc, mb * mc, eb + ec);
        let e0 = ea.min(ep);
        let term = |neg: bool, m: i128, shift: i32| {
            let v = m << shift;
            if neg { -v } else { v }
        };
        let sum = term(sa, ma, ea - e0) + term(sp, mp, ep - e0);
        let sign = if sum < 0 { 0x8000 } else { 0 };
        let u = sum.unsigned_abs();
        if u == 0 {
            return 0;
        }
        // Round to the fp16 grid: 11 significant bits at normal values, 2^-24 below 2^-14.
        let top = 127 - u.leading_zeros() as i32 + e0;
        let q = (top - 10).max(-24);
        let shift = q - e0;
        let mut mant = if shift <= 0 {
            u << -shift
        } else {
            let (kept, rest, half) = (u >> shift, u & ((1 << shift) - 1), 1 << (shift - 1));
            kept + u128::from(rest > half || (rest == half && kept & 1 == 1))
        };
        let mut q = q;
        if mant == 2048 {
            mant = 1024;
            q += 1;
        }
        if mant == 0 {
            return sign;
        }
        let mant = mant as u16;
        if mant < 1024 {
            return sign | mant;
        }
        let e = q + 25;
        if e > 30 {
            return sign | INF;
        }
        sign | ((e as u16) << 10) | (mant - 1024)
    }

    /// The software `fma` equals the exact fused result on finite inputs across the whole
    /// range (the fp16 leaves are fused: NEON `fmla`, RVV `vfmacc`). The oracle computes the
    /// product exactly and the sum in f64: that is one rounding, not two, for these inputs.
    #[test]
    fn fma_is_the_exact_fused_result() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        // A finite, non-zero fp16 in any binade, subnormals included.
        let mut finite = || loop {
            let h = next() as u16;
            if (h >> 10) & 0x1F != 0x1F && h & 0x7FFF != 0 {
                return h;
            }
        };
        let mut checked = 0u32;
        for _ in 0..400_000 {
            let (a, b, c) = (finite(), finite(), finite());
            assert_eq!(
                fma(a, b, c),
                exact_fma(a, b, c),
                "fma({a:#06x}, {b:#06x}, {c:#06x})"
            );
            checked += 1;
        }
        // Cancellation: a + b c with the product equal to -a (the exact sum is then zero or
        // the tiny residue the rounding keeps).
        for _ in 0..50_000 {
            let (a, b) = (finite(), finite());
            let c = from_f64(-to_f64(a) / to_f64(b));
            if (c >> 10) & 0x1F == 0x1F || c & 0x7FFF == 0 {
                continue;
            }
            assert_eq!(
                fma(a, b, c),
                exact_fma(a, b, c),
                "cancel {a:#06x} {b:#06x} {c:#06x}"
            );
            checked += 1;
        }
        eprintln!(
            "fma: {checked} random triples equal the exact fused result (fp16 hardware here: {})",
            crate::isp::half::hardware()
        );
    }
}
