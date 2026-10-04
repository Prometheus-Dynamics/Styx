//! Float functions `core` lacks, for `no_std` builds: [`Float`] gives `f64` the methods of
//! std's this crate uses, through `libm`. With `std` the inherent methods are used (the trait
//! is not imported), so std builds are unchanged.

#![cfg_attr(feature = "std", allow(dead_code))]

/// The std float methods this crate uses, for `no_std` builds.
pub(crate) trait Float: Sized {
    fn round(self) -> Self;
    fn fract(self) -> Self;
    fn powi(self, n: i32) -> Self;
}

impl Float for f64 {
    #[inline]
    fn round(self) -> f64 {
        libm::round(self)
    }
    #[inline]
    fn fract(self) -> f64 {
        self - libm::trunc(self)
    }
    #[inline]
    fn powi(self, n: i32) -> f64 {
        // compiler-rt's `__powidf2`, what std's `powi` calls.
        let mut base = self;
        let mut r = 1.0;
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
}
