//! The H.264 six-tap half-pel filter (8.4.2.2.1) over rows, and the rounded mean of two
//! blocks that makes quarter positions: an encoder's half-pel planes of a reference picture.
//! Rows are taken whole, so the caller lays out the clamped edges.

use super::super::SimdBackend;

/// The six-tap sums (`E − 5F + 20G + 20H − 5I + J`) of `src` at each output position `x`
/// over `src[x..x + 6]` into `sums`, and the half-pel samples `clip((sum + 16) >> 5)` into
/// `half`. `src` holds `sums.len() + 5` samples, `half` at least `sums.len()`.
pub fn hpel_h(src: &[u8], sums: &mut [i16], half: &mut [u8]) -> SimdBackend {
    let n = sums.len();
    assert!(src.len() >= n + 5 && half.len() >= n);
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if super::super::rvv::h264_hpel::hpel_h(src, sums, half) {
        return SimdBackend::Rvv;
    }
    hpel_h_scalar(src, sums, half);
    SimdBackend::Scalar
}

/// The scalar [`hpel_h`].
pub fn hpel_h_scalar(src: &[u8], sums: &mut [i16], half: &mut [u8]) {
    for (x, (s, h)) in sums.iter_mut().zip(half.iter_mut()).enumerate() {
        let p = |k: usize| i32::from(src[x + k]);
        let v = p(0) + p(5) - 5 * (p(1) + p(4)) + 20 * (p(2) + p(3));
        *s = v as i16;
        *h = ((v + 16) >> 5).clamp(0, 255) as u8;
    }
}

/// The vertical half-pel samples between rows: `out[x]` from `rows[0..6][x]`, rounded and
/// clipped as [`hpel_h`]'s. Each row holds at least `out.len()` samples.
pub fn hpel_v(rows: [&[u8]; 6], out: &mut [u8]) -> SimdBackend {
    assert!(rows.iter().all(|r| r.len() >= out.len()));
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if super::super::rvv::h264_hpel::hpel_v(rows, out) {
        return SimdBackend::Rvv;
    }
    hpel_v_scalar(rows, out);
    SimdBackend::Scalar
}

/// The scalar [`hpel_v`].
pub fn hpel_v_scalar(rows: [&[u8]; 6], out: &mut [u8]) {
    for (x, o) in out.iter_mut().enumerate() {
        let p = |k: usize| i32::from(rows[k][x]);
        let v = p(0) + p(5) - 5 * (p(1) + p(4)) + 20 * (p(2) + p(3));
        *o = ((v + 16) >> 5).clamp(0, 255) as u8;
    }
}

/// The centre half-pel samples: the six-tap filter across six rows of [`hpel_h`]'s sums,
/// `clip((sum + 512) >> 10)`. Each row holds at least `out.len()` sums.
pub fn hpel_j(rows: [&[i16]; 6], out: &mut [u8]) -> SimdBackend {
    assert!(rows.iter().all(|r| r.len() >= out.len()));
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if super::super::rvv::h264_hpel::hpel_j(rows, out) {
        return SimdBackend::Rvv;
    }
    hpel_j_scalar(rows, out);
    SimdBackend::Scalar
}

/// The scalar [`hpel_j`].
pub fn hpel_j_scalar(rows: [&[i16]; 6], out: &mut [u8]) {
    for (x, o) in out.iter_mut().enumerate() {
        let p = |k: usize| i32::from(rows[k][x]);
        let v = p(0) + p(5) - 5 * (p(1) + p(4)) + 20 * (p(2) + p(3));
        *o = ((v + 512) >> 10).clamp(0, 255) as u8;
    }
}

/// The rounded-up mean `(a + b + 1) >> 1` of two 16x16 blocks (rows `a_stride` and
/// `b_stride` apart) into `out`.
pub fn avg16(
    a: &[u8],
    a_stride: usize,
    b: &[u8],
    b_stride: usize,
    out: &mut [u8; 256],
) -> SimdBackend {
    assert!(a.len() >= 15 * a_stride + 16 && b.len() >= 15 * b_stride + 16);
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if super::super::rvv::h264_hpel::avg16(a, a_stride, b, b_stride, out) {
        return SimdBackend::Rvv;
    }
    avg16_scalar(a, a_stride, b, b_stride, out);
    SimdBackend::Scalar
}

/// The scalar [`avg16`].
pub fn avg16_scalar(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize, out: &mut [u8; 256]) {
    for r in 0..16 {
        let (x, y) = (&a[r * a_stride..][..16], &b[r * b_stride..][..16]);
        for ((o, &p), &q) in out[r * 16..][..16].iter_mut().zip(x).zip(y) {
            *o = ((u16::from(p) + u16::from(q) + 1) >> 1) as u8;
        }
    }
}
