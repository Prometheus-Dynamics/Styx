//! IEEE 754 half precision (fp16) arithmetic in software, bit for bit what AArch64's FP16
//! instructions compute with the default rounding (to nearest, ties to even, subnormals kept):
//! each operation is done exactly in `f64` and rounded once. The oracle of the
//! [`Arithmetic::Half`](crate::Arithmetic::Half) kernels, and their fallback without FP16
//! hardware.
//!
//! Exactness: sums and products of two fp16 values are exact in `f64`, and `a + b c` is exact
//! as long as the terms span fewer than 53 bits (always for the values the ISP produces:
//! magnitudes between 2^-24 and 2^18).

#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

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
}
