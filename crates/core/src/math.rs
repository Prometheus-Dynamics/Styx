//! Float functions `core` lacks, for `no_std` builds: the shim every Styx crate (and its
//! consumers) can share.
//!
//! [`Float`] gives `f32` / `f64` std's float methods under the same names, through `libm`.
//! Bring it into scope unnamed (`use styx_core::math::Float as _;`) and write `x.sqrt()` as with
//! std. Inherent methods win over trait methods, so wherever `std` is linked the std methods
//! run exactly as before and the trait only answers in a build without `std`; importing it only
//! there (`#[cfg(not(feature = "std"))]`) avoids an unused-import warning, nothing else. (std's
//! methods are found wherever std is in the build at all, e.g. a `no_std` crate's host tests,
//! or a dependency that links it.)
//!
//! Exact either way (IEEE results that do not depend on the library, bit for bit the same as
//! std's): `sqrt`, `floor`, `ceil`, `round`, `round_ties_even`, `trunc`, `fract`, `mul_add` (a
//! fused multiply-add), `rem_euclid`, `div_euclid`, and `powi` (the square-and-multiply sequence
//! compiler-rt's `__powisf2` / `__powidf2` run for std). `abs`, `min`, `max`, `clamp`,
//! `signum`, `copysign`, `recip`, `to_degrees` and `to_radians` are `core` methods and need
//! nothing here.
//!
//! Transcendental functions (`exp`, `exp2`, `ln`, `ln_1p`, `log2`, `log10`, `powf`, `cbrt`,
//! `hypot`, `sin`, `cos`, `tan`, `sin_cos`, `asin`, `acos`, `atan`, `atan2`, `tanh`) are libm's:
//! they can differ from the platform's libm in the last bit: within 1 ulp, `tanh` within 3
//! (the tests compare both with glibc's on the host, `f32` and `f64`).
//!
//! The trait is sealed (implemented for `f32` and `f64` only), so later releases can add
//! methods without breaking anyone.
//!
//! ```
//! #[allow(unused_imports)]
//! use styx_core::math::Float as _;
//!
//! assert_eq!(2.0f32.sqrt(), core::f32::consts::SQRT_2);
//! assert_eq!(styx_core::math::Float::round(2.5f64), 3.0);
//! ```

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// std's float methods for `f32` / `f64` without `std` (`libm`); see the [module](self) for
/// which are exact and how far the others can be from std's.
pub trait Float: Sized + sealed::Sealed {
    /// The square root (exact).
    fn sqrt(self) -> Self;
    /// The largest integer `<= self` (exact).
    fn floor(self) -> Self;
    /// The smallest integer `>= self` (exact).
    fn ceil(self) -> Self;
    /// The nearest integer, halves away from zero (exact).
    fn round(self) -> Self;
    /// The nearest integer, halves to even (exact).
    fn round_ties_even(self) -> Self;
    /// The integer part (exact).
    fn trunc(self) -> Self;
    /// `self - self.trunc()` (exact).
    fn fract(self) -> Self;
    /// `self * a + b` with one rounding (exact).
    fn mul_add(self, a: Self, b: Self) -> Self;
    /// The least non-negative remainder of `self / rhs` (exact).
    fn rem_euclid(self, rhs: Self) -> Self;
    /// The quotient of Euclidean division (exact).
    fn div_euclid(self, rhs: Self) -> Self;
    /// `self` to an integer power (the same multiplications as std's).
    fn powi(self, n: i32) -> Self;
    /// `self` to a float power (within 1 ulp).
    fn powf(self, n: Self) -> Self;
    /// `e^self` (within 1 ulp).
    fn exp(self) -> Self;
    /// `2^self` (within 1 ulp).
    fn exp2(self) -> Self;
    /// The natural logarithm (within 1 ulp).
    fn ln(self) -> Self;
    /// `ln(1 + self)`, accurate near zero (within 1 ulp).
    fn ln_1p(self) -> Self;
    /// The base-2 logarithm (within 1 ulp).
    fn log2(self) -> Self;
    /// The base-10 logarithm (within 1 ulp).
    fn log10(self) -> Self;
    /// The cube root (within 1 ulp).
    fn cbrt(self) -> Self;
    /// `sqrt(self² + other²)` without overflow on the way (within 1 ulp).
    fn hypot(self, other: Self) -> Self;
    /// The sine, in radians (within 1 ulp).
    fn sin(self) -> Self;
    /// The cosine, in radians (within 1 ulp).
    fn cos(self) -> Self;
    /// The tangent, in radians (within 1 ulp).
    fn tan(self) -> Self;
    /// `(sin, cos)` at once (within 1 ulp).
    fn sin_cos(self) -> (Self, Self);
    /// The arcsine, in radians (within 1 ulp).
    fn asin(self) -> Self;
    /// The arccosine, in radians (within 1 ulp).
    fn acos(self) -> Self;
    /// The arctangent, in radians (within 1 ulp).
    fn atan(self) -> Self;
    /// The four-quadrant arctangent of `self / other` (`self` is y), in radians (within 1 ulp).
    fn atan2(self, other: Self) -> Self;
    /// The hyperbolic tangent (within 3 ulps).
    fn tanh(self) -> Self;
}

macro_rules! float_impl {
    ($t:ty, $sqrt:ident, $floor:ident, $ceil:ident, $round:ident, $roundeven:ident,
     $trunc:ident, $fma:ident, $pow:ident, $exp:ident, $exp2:ident, $ln:ident, $ln1p:ident,
     $log2:ident, $log10:ident, $cbrt:ident, $hypot:ident, $sin:ident, $cos:ident, $tan:ident,
     $sincos:ident, $asin:ident, $acos:ident, $atan:ident, $atan2:ident, $tanh:ident) => {
        impl Float for $t {
            #[inline]
            fn sqrt(self) -> $t {
                libm::$sqrt(self)
            }
            #[inline]
            fn floor(self) -> $t {
                libm::$floor(self)
            }
            #[inline]
            fn ceil(self) -> $t {
                libm::$ceil(self)
            }
            #[inline]
            fn round(self) -> $t {
                libm::$round(self)
            }
            #[inline]
            fn round_ties_even(self) -> $t {
                libm::$roundeven(self)
            }
            #[inline]
            fn trunc(self) -> $t {
                libm::$trunc(self)
            }
            #[inline]
            fn fract(self) -> $t {
                // As std.
                self - libm::$trunc(self)
            }
            #[inline]
            fn mul_add(self, a: $t, b: $t) -> $t {
                libm::$fma(self, a, b)
            }
            #[inline]
            fn rem_euclid(self, rhs: $t) -> $t {
                // As std.
                let r = self % rhs;
                if r < 0.0 { r + rhs.abs() } else { r }
            }
            #[inline]
            fn div_euclid(self, rhs: $t) -> $t {
                // As std.
                let q = libm::$trunc(self / rhs);
                if self % rhs < 0.0 {
                    return if rhs > 0.0 { q - 1.0 } else { q + 1.0 };
                }
                q
            }
            #[inline]
            fn powi(self, n: i32) -> $t {
                // compiler-rt's `__powisf2` / `__powidf2`, which std's `powi` lowers to, and the
                // order LLVM expands a constant exponent in.
                let mut base = self;
                let mut r: $t = 1.0;
                let mut e = n;
                loop {
                    if e & 1 != 0 {
                        r *= base;
                    }
                    e /= 2;
                    if e == 0 {
                        break;
                    }
                    base *= base;
                }
                if n < 0 { 1.0 / r } else { r }
            }
            #[inline]
            fn powf(self, n: $t) -> $t {
                libm::$pow(self, n)
            }
            #[inline]
            fn exp(self) -> $t {
                libm::$exp(self)
            }
            #[inline]
            fn exp2(self) -> $t {
                libm::$exp2(self)
            }
            #[inline]
            fn ln(self) -> $t {
                libm::$ln(self)
            }
            #[inline]
            fn ln_1p(self) -> $t {
                libm::$ln1p(self)
            }
            #[inline]
            fn log2(self) -> $t {
                libm::$log2(self)
            }
            #[inline]
            fn log10(self) -> $t {
                libm::$log10(self)
            }
            #[inline]
            fn cbrt(self) -> $t {
                libm::$cbrt(self)
            }
            #[inline]
            fn hypot(self, other: $t) -> $t {
                libm::$hypot(self, other)
            }
            #[inline]
            fn sin(self) -> $t {
                libm::$sin(self)
            }
            #[inline]
            fn cos(self) -> $t {
                libm::$cos(self)
            }
            #[inline]
            fn tan(self) -> $t {
                libm::$tan(self)
            }
            #[inline]
            fn sin_cos(self) -> ($t, $t) {
                libm::$sincos(self)
            }
            #[inline]
            fn asin(self) -> $t {
                libm::$asin(self)
            }
            #[inline]
            fn acos(self) -> $t {
                libm::$acos(self)
            }
            #[inline]
            fn atan(self) -> $t {
                libm::$atan(self)
            }
            #[inline]
            fn atan2(self, other: $t) -> $t {
                libm::$atan2(self, other)
            }
            #[inline]
            fn tanh(self) -> $t {
                libm::$tanh(self)
            }
        }
    };
}

float_impl!(
    f32, sqrtf, floorf, ceilf, roundf, roundevenf, truncf, fmaf, powf, expf, exp2f, logf, log1pf,
    log2f, log10f, cbrtf, hypotf, sinf, cosf, tanf, sincosf, asinf, acosf, atanf, atan2f, tanhf
);
float_impl!(
    f64, sqrt, floor, ceil, round, roundeven, trunc, fma, pow, exp, exp2, log, log1p, log2, log10,
    cbrt, hypot, sin, cos, tan, sincos, asin, acos, atan, atan2, tanh
);

#[cfg(test)]
mod tests;
