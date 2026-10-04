//! Float functions `core` lacks, for `no_std` builds.
//!
//! [`Float`] gives `f32` / `f64` the std methods this crate calls, through `libm`. Modules
//! import it only without `std` (`#[cfg(not(feature = "std"))] use crate::math::Float as _;`):
//! with `std` the inherent methods are used, so std builds are unchanged.

// Without std not every method is used by every build; with std none are.
#![allow(dead_code)]

/// The std float methods this crate uses, for `no_std` builds (`libm`).
pub(crate) trait Float: Sized {
    fn sqrt(self) -> Self;
    fn round(self) -> Self;
    fn round_ties_even(self) -> Self;
    fn floor(self) -> Self;
    fn ceil(self) -> Self;
    fn trunc(self) -> Self;
    fn fract(self) -> Self;
    fn powi(self, n: i32) -> Self;
    fn powf(self, e: Self) -> Self;
    fn exp(self) -> Self;
    fn exp2(self) -> Self;
    fn ln(self) -> Self;
    fn log2(self) -> Self;
    fn log10(self) -> Self;
    fn sin(self) -> Self;
    fn cos(self) -> Self;
    fn atan2(self, x: Self) -> Self;
    fn hypot(self, y: Self) -> Self;
    fn mul_add(self, a: Self, b: Self) -> Self;
    fn rem_euclid(self, rhs: Self) -> Self;
}

macro_rules! float_impl {
    ($t:ty, $sqrt:ident, $round:ident, $roundeven:ident, $floor:ident, $ceil:ident,
     $trunc:ident, $pow:ident, $exp:ident, $exp2:ident, $ln:ident, $log2:ident,
     $log10:ident, $sin:ident, $cos:ident, $atan2:ident, $hypot:ident, $fma:ident) => {
        impl Float for $t {
            #[inline]
            fn sqrt(self) -> $t {
                libm::$sqrt(self)
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
            fn floor(self) -> $t {
                libm::$floor(self)
            }
            #[inline]
            fn ceil(self) -> $t {
                libm::$ceil(self)
            }
            #[inline]
            fn trunc(self) -> $t {
                libm::$trunc(self)
            }
            #[inline]
            fn fract(self) -> $t {
                self - libm::$trunc(self)
            }
            #[inline]
            fn powi(self, n: i32) -> $t {
                // compiler-rt's `__powidf2` / `__powisf2`, what std's `powi` calls.
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
            fn powf(self, e: $t) -> $t {
                libm::$pow(self, e)
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
            fn log2(self) -> $t {
                libm::$log2(self)
            }
            #[inline]
            fn log10(self) -> $t {
                libm::$log10(self)
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
            fn atan2(self, x: $t) -> $t {
                libm::$atan2(self, x)
            }
            #[inline]
            fn hypot(self, y: $t) -> $t {
                libm::$hypot(self, y)
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
        }
    };
}

float_impl!(
    f64, sqrt, round, roundeven, floor, ceil, trunc, pow, exp, exp2, log, log2, log10, sin, cos,
    atan2, hypot, fma
);
float_impl!(
    f32, sqrtf, roundf, roundevenf, floorf, ceilf, truncf, powf, expf, exp2f, logf, log2f, log10f,
    sinf, cosf, atan2f, hypotf, fmaf
);
