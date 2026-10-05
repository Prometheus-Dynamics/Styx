//! The host tests link std, so std's inherent methods and the libm ones are compared directly
//! (`Float::method(x)` names the trait's) for `f32` and `f64`: the exact functions bit for bit,
//! the transcendental ones within the documented tolerance (1 ulp; `tanh` 3).

use super::Float;

macro_rules! float_tests {
    ($m:ident, $t:ty, $bits:ty, $ibits:ty, $mant:expr) => {
        mod $m {
            use super::Float;

            fn samples() -> impl Iterator<Item = $t> {
                let specials: [$t; 16] = [
                    0.0,
                    -0.0,
                    0.5,
                    -0.5,
                    1.5,
                    -1.5,
                    2.5,
                    -2.5,
                    <$t>::MIN_POSITIVE,
                    <$t>::MAX,
                    <$t>::MIN,
                    <$t>::EPSILON,
                    <$t>::INFINITY,
                    <$t>::NEG_INFINITY,
                    $mant - 0.5,
                    -($mant + 1.0),
                ];
                let n = if cfg!(miri) { 200u32 } else { 20_000 };
                let sweep = (0..n).map(move |i| {
                    let t = i as $t / n as $t;
                    (t - 0.5) * 2_000.0 + t * t * 0.37
                });
                let bits = (0..n / 5).map(|i| {
                    let b = (i as $bits).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as $bits) >> 1;
                    <$t>::from_bits(b)
                });
                specials.into_iter().chain(sweep).chain(bits)
            }

            fn same(a: $t, b: $t) -> bool {
                a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
            }

            fn ulps(a: $t, b: $t) -> $bits {
                if same(a, b) {
                    return 0;
                }
                if !a.is_finite() || !b.is_finite() {
                    return <$bits>::MAX;
                }
                let key = |x: $t| {
                    let bits = x.to_bits() as $ibits;
                    if bits < 0 { <$ibits>::MIN - bits } else { bits }
                };
                key(a).abs_diff(key(b))
            }

            #[test]
            fn exact_functions_match_std_bit_for_bit() {
                for x in samples() {
                    assert!(same(Float::sqrt(x), x.sqrt()), "sqrt {x}");
                    assert!(same(Float::floor(x), x.floor()), "floor {x}");
                    assert!(same(Float::ceil(x), x.ceil()), "ceil {x}");
                    assert!(same(Float::round(x), x.round()), "round {x}");
                    assert!(
                        same(Float::round_ties_even(x), x.round_ties_even()),
                        "round_ties_even {x}"
                    );
                    assert!(same(Float::trunc(x), x.trunc()), "trunc {x}");
                    assert!(same(Float::fract(x), x.fract()), "fract {x}");
                    for y in [3.0, -3.0, 0.7, 255.0, 360.0] {
                        assert!(
                            same(Float::rem_euclid(x, y), x.rem_euclid(y)),
                            "rem_euclid {x} {y}"
                        );
                        assert!(
                            same(Float::div_euclid(x, y), x.div_euclid(y)),
                            "div_euclid {x} {y}"
                        );
                        assert!(
                            same(Float::mul_add(x, y, 0.25), x.mul_add(y, 0.25)),
                            "mul_add {x}"
                        );
                    }
                    for n in [-3, -2, -1, 0, 1, 2, 3, 4, 7] {
                        // std's powi goes through compiler-rt (or LLVM's expansion of a
                        // constant exponent): the same multiplications.
                        let n = core::hint::black_box(n);
                        assert!(same(Float::powi(x, n), x.powi(n)), "powi {x} {n}");
                    }
                }
            }

            /// libm's transcendental functions agree with the platform's within one unit in
            /// the last place on these samples (most are identical; `tanh` within three).
            #[test]
            fn transcendental_within_one_ulp() {
                for x in samples().filter(|x| x.is_finite() && x.abs() < 1.0e4) {
                    let (s, c) = Float::sin_cos(x);
                    let pairs = [
                        ("exp", Float::exp(x / 100.0), (x / 100.0).exp()),
                        ("exp2", Float::exp2(x / 100.0), (x / 100.0).exp2()),
                        ("ln", Float::ln(x.abs() + 1.0), (x.abs() + 1.0).ln()),
                        ("log2", Float::log2(x.abs() + 1.0), (x.abs() + 1.0).log2()),
                        (
                            "log10",
                            Float::log10(x.abs() + 1.0),
                            (x.abs() + 1.0).log10(),
                        ),
                        ("ln_1p", Float::ln_1p(x.abs()), x.abs().ln_1p()),
                        ("powf", Float::powf(x.abs(), 0.45), x.abs().powf(0.45)),
                        ("cbrt", Float::cbrt(x), x.cbrt()),
                        ("sin", Float::sin(x), x.sin()),
                        ("cos", Float::cos(x), x.cos()),
                        ("sin_cos.0", s, x.sin()),
                        ("sin_cos.1", c, x.cos()),
                        ("tan", Float::tan(x / 1.0e4), (x / 1.0e4).tan()),
                        ("tanh", Float::tanh(x / 1.0e3), (x / 1.0e3).tanh()),
                        ("atan2", Float::atan2(x, 3.0), x.atan2(3.0)),
                        ("atan", Float::atan(x), x.atan()),
                        ("asin", Float::asin(x / 1.0e4), (x / 1.0e4).asin()),
                        ("acos", Float::acos(x / 1.0e4), (x / 1.0e4).acos()),
                        ("hypot", Float::hypot(x, 3.0), x.hypot(3.0)),
                    ];
                    for (name, ours, theirs) in pairs {
                        // The documented tolerance: 1 ulp, `tanh` 3.
                        let tolerance = if name == "tanh" { 3 } else { 1 };
                        assert!(
                            ulps(ours, theirs) <= tolerance,
                            "{name} {x}: {ours} vs {theirs}"
                        );
                    }
                }
            }
        }
    };
}

float_tests!(f32_vs_std, f32, u32, i32, 8_388_608.0);
float_tests!(f64_vs_std, f64, u64, i64, 4_503_599_627_370_496.0);

#[test]
fn the_trait_answers_where_std_is_not_linked() {
    // Named through the trait, as a `no_std` build resolves `x.sqrt()`.
    assert_eq!(Float::sqrt(16.0f32), 4.0);
    assert_eq!(Float::powi(3.0f64, 3), 27.0);
    assert_eq!(Float::div_euclid(-7.0f32, 2.0), -4.0);
}
