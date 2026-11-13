//! The kernel policy switch, in its own test binary: it changes process-wide state, so it must
//! not share a process with the unit tests that assume the default.

use styx_simd::{SimdBackend, SimdPolicy, box2_row, policy, scalar, set_policy, strongest_backend};

/// Deterministic random bytes (xorshift64).
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn scalar_policy_runs_the_oracle_and_auto_restores_the_vector_leaves() {
    assert_eq!(policy(), SimdPolicy::Auto, "the default is Auto");

    let width = 301;
    let src = bytes(width * 4, 7);
    let top = bytes(width * 2 + 2, 8);
    let bottom = bytes(width * 2 + 2, 9);

    // Auto: the vector leaves may run; whatever they do, the output is the oracle's.
    let mut auto = vec![0u8; width * 3];
    let mut want = vec![0u8; width * 3];
    let backend = styx_simd::x32_to_rgb24_row(&src, &mut auto, width, true);
    scalar::x32_to_rgb24_row(&src, &mut want, width, true);
    assert_eq!(auto, want);
    if cfg!(target_arch = "x86_64") {
        assert_ne!(
            backend,
            SimdBackend::Scalar,
            "Auto on x86-64 runs a vector leaf"
        );
        assert_ne!(strongest_backend(), SimdBackend::Scalar);
    }

    // Scalar: every dispatcher reports the scalar backend and still matches the oracle.
    set_policy(SimdPolicy::Scalar);
    assert_eq!(policy(), SimdPolicy::Scalar);
    assert_eq!(strongest_backend(), SimdBackend::Scalar);

    let mut got = vec![0u8; width * 3];
    let backend = styx_simd::x32_to_rgb24_row(&src, &mut got, width, true);
    assert_eq!(backend, SimdBackend::Scalar);
    assert_eq!(got, want);

    let mut got = vec![0u8; width];
    let mut want = vec![0u8; width];
    let backend = box2_row(&top, &bottom, &mut got, width);
    scalar::box2_row(&top, &bottom, &mut want, width);
    assert_eq!(backend, SimdBackend::Scalar);
    assert_eq!(got, want);

    set_policy(SimdPolicy::Auto);
    assert_eq!(policy(), SimdPolicy::Auto);
}
