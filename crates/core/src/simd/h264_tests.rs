//! The H.264 kernels (`simd::h264`): the dispatchers (vector leaves where the CPU has them)
//! against the scalar oracle over grid shapes, strides and quantisers, with guard bytes; and
//! the transforms against their matrix definitions.

use super::SimdBackend;
use super::h264::*;

/// The backend a dispatcher should report here: RVV wherever the target is riscv64 (the
/// tests run under QEMU with vectors), scalar elsewhere.
fn expected() -> SimdBackend {
    if cfg!(all(feature = "rvv", target_arch = "riscv64")) {
        SimdBackend::Rvv
    } else {
        SimdBackend::Scalar
    }
}

/// Deterministic random bytes.
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

/// H.264's quantiser at `qp` (multipliers by position class), intra or inter rounding.
fn quant(qp: u32, intra: bool) -> Quant4 {
    const MF: [[i32; 3]; 6] = [
        [13107, 5243, 8066],
        [11916, 4660, 7490],
        [10082, 4194, 6554],
        [9362, 3647, 5825],
        [8192, 3355, 5243],
        [7282, 2893, 4559],
    ];
    let qbits = 15 + qp / 6;
    let mf = core::array::from_fn(|i| {
        let (x, y) = (i & 3, i >> 2);
        MF[(qp % 6) as usize][match (x & 1, y & 1) {
            (0, 0) => 0,
            (1, 1) => 1,
            _ => 2,
        }]
    });
    Quant4 {
        mf,
        f: (1 << qbits) / if intra { 3 } else { 6 },
        qbits,
    }
}

const GRIDS: [(usize, usize); 5] = [(4, 4), (2, 2), (1, 1), (4, 1), (3, 2)];

#[test]
fn sad_matches_the_oracle() {
    for (i, (sa, sb)) in [(16, 16), (17, 40), (64, 23)].into_iter().enumerate() {
        let a = bytes(15 * sa + 16, i as u64);
        let b = bytes(15 * sb + 16, !i as u64);
        assert_eq!(sad16(&a, sa, &b, sb), sad16_scalar(&a, sa, &b, sb));
        // The extremes: every byte 255 apart.
        let (z, f) = (vec![0u8; 15 * sa + 16], vec![255u8; 15 * sb + 16]);
        assert_eq!(sad16(&z, sa, &f, sb), 16 * 16 * 255);
    }
}

#[test]
fn residual_quant_matches_the_oracle() {
    for (g, &grid) in GRIDS.iter().enumerate() {
        for (stride, pstride) in [(4 * grid.0, 4 * grid.0), (4 * grid.0 + 13, 16)] {
            let pstride = pstride.max(4 * grid.0);
            let len = |s: usize| (4 * grid.1 - 1) * s + 4 * grid.0;
            let src = bytes(len(stride), g as u64 * 7 + stride as u64);
            let mut pred = bytes(len(pstride), g as u64 * 11 + 3);
            if stride == pstride {
                // Extremes: full-scale residuals of both signs.
                for (p, s) in pred.iter_mut().zip(&src) {
                    *p = if *s > 127 { 0 } else { 255 };
                }
            }
            let n = grid.0 * grid.1;
            for qp in [0, 1, 17, 28, 39, 51] {
                for (intra, ac_only) in [(true, false), (false, false), (true, true)] {
                    let q = quant(qp, intra);
                    let (mut dc, mut lv) = (vec![7i32; n + 1], vec![[7i32; 16]; n + 1]);
                    let (mut dc2, mut lv2) = (dc.clone(), lv.clone());
                    let backend = residual_quant(
                        &src, stride, &pred, pstride, grid, &q, ac_only, &mut dc, &mut lv,
                    );
                    assert_eq!(backend, expected());
                    residual_quant_scalar(
                        &src, stride, &pred, pstride, grid, &q, ac_only, &mut dc2, &mut lv2,
                    );
                    assert_eq!(lv, lv2, "{grid:?} qp {qp} intra {intra} ac {ac_only}");
                    assert_eq!(dc, dc2, "{grid:?} qp {qp}");
                    assert_eq!((dc[n], lv[n]), (7, [7; 16]), "guard");
                }
            }
        }
    }
}

#[test]
fn recon_matches_the_oracle() {
    for (g, &grid) in GRIDS.iter().enumerate() {
        let n = grid.0 * grid.1;
        let (pstride, dstride) = (4 * grid.0 + 5, 4 * grid.0 + 2);
        let pred = bytes((4 * grid.1 - 1) * pstride + 4 * grid.0, g as u64);
        for qp in [0u32, 13, 30, 51] {
            const V: [[i32; 3]; 6] = [
                [10, 16, 13],
                [11, 18, 14],
                [13, 20, 16],
                [14, 23, 18],
                [16, 25, 20],
                [18, 29, 23],
            ];
            let scale: [i32; 16] = core::array::from_fn(|i| {
                let c = match ((i & 3) & 1, (i >> 2) & 1) {
                    (0, 0) => 0,
                    (1, 1) => 1,
                    _ => 2,
                };
                V[(qp % 6) as usize][c] << (qp / 6)
            });
            let r = bytes(n * 16 * 2, qp as u64 + g as u64);
            // Levels from small to the clamp, both signs.
            let levels: Vec<[i32; 16]> = (0..n)
                .map(|b| {
                    core::array::from_fn(|i| {
                        let v = i32::from(r[b * 32 + 2 * i]) - 128;
                        if r[b * 32 + 2 * i + 1] < 8 {
                            v * 16
                        } else {
                            v / 16
                        }
                    })
                })
                .collect();
            let dc: Vec<i32> = (0..n).map(|b| (i32::from(r[b]) - 128) * 300).collect();
            for dc in [None, Some(&dc[..])] {
                let len = (4 * grid.1 - 1) * dstride + 4 * grid.0;
                let (mut a, mut b) = (vec![0xA5u8; len + 1], vec![0xA5u8; len + 1]);
                let backend = recon(&levels, &scale, dc, &pred, pstride, &mut a, dstride, grid);
                assert_eq!(backend, expected());
                recon_scalar(&levels, &scale, dc, &pred, pstride, &mut b, dstride, grid);
                assert_eq!(a, b, "{grid:?} qp {qp} dc {}", dc.is_some());
                assert_eq!(a[len], 0xA5, "guard");
            }
        }
    }
}

#[test]
fn transforms_match_their_matrices() {
    let r: [i32; 16] = core::array::from_fn(|i| (i as i32 * 73 % 511) - 255);
    let cf = [[1, 1, 1, 1], [2, 1, -1, -2], [1, -1, -1, 1], [1, -2, 2, -1]];
    let got = fdct4(&r);
    for v in 0..4 {
        for u in 0..4 {
            let want: i32 = (0..4)
                .flat_map(|y| (0..4).map(move |x| (y, x)))
                .map(|(y, x)| cf[v][y] * r[y * 4 + x] * cf[u][x])
                .sum();
            assert_eq!(got[v * 4 + u], want, "({v}, {u})");
        }
    }
    // The inverse of a lone DC is flat: (d + 32) >> 6 everywhere.
    let mut d = [0; 16];
    d[0] = 640;
    assert_eq!(idct4(&d), [10; 16]);
}

#[test]
fn full_search_finds_a_shift() {
    let (w, h) = (64, 48);
    let reference = bytes(w * h, 99);
    // `cur` is `reference` moved 3 right and 2 down: the block at (16, 16) came from (13, 14).
    let cur: Vec<u8> = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as i32 - 3, (i / w) as i32 - 2);
            reference[(y.clamp(0, h as i32 - 1) as usize) * w + x.clamp(0, w as i32 - 1) as usize]
        })
        .collect();
    assert_eq!(
        full_search(&cur, &reference, (w, h), (1, 1), 8),
        ([-3, -2], 0)
    );
    // A flat picture ties everywhere: the centre wins.
    let flat = vec![9u8; w * h];
    assert_eq!(full_search(&flat, &flat, (w, h), (2, 1), 16), ([0, 0], 0));
}

#[test]
fn half_pel_filters_match_the_oracle() {
    for (i, n) in [1usize, 7, 16, 33, 255, 1296].into_iter().enumerate() {
        for extreme in [false, true] {
            let mut src = bytes(n + 5 + 3, i as u64);
            if extreme {
                // Full-scale steps: the sums' ends and the clipping both ways.
                for (k, v) in src.iter_mut().enumerate() {
                    *v = if (k / 2) % 2 == 0 { 255 } else { 0 };
                }
            }
            let (mut s, mut h) = (vec![7i16; n + 1], vec![7u8; n + 1]);
            let (mut s2, mut h2) = (s.clone(), h.clone());
            assert_eq!(hpel_h(&src, &mut s[..n], &mut h[..n]), expected());
            hpel_h_scalar(&src, &mut s2[..n], &mut h2[..n]);
            assert_eq!((&s, &h), (&s2, &h2), "h n {n}");
            assert_eq!((s[n], h[n]), (7, 7), "guard");

            let rows: Vec<Vec<u8>> = (0..6)
                .map(|k| {
                    let r = bytes(n, (i * 6 + k) as u64 + 50);
                    if extreme {
                        r.iter()
                            .map(|&v| if (v > 127) ^ (k % 2 == 0) { 255 } else { 0 })
                            .collect()
                    } else {
                        r
                    }
                })
                .collect();
            let rows: [&[u8]; 6] = core::array::from_fn(|k| &rows[k][..]);
            let (mut o, mut o2) = (vec![7u8; n + 1], vec![7u8; n + 1]);
            assert_eq!(hpel_v(rows, &mut o[..n]), expected());
            hpel_v_scalar(rows, &mut o2[..n]);
            assert_eq!(o, o2, "v n {n}");

            // Sums over the filter's whole range, -2550 to 10710.
            let sums: Vec<Vec<i16>> = (0..6)
                .map(|k| {
                    bytes(2 * n, (i * 6 + k) as u64 + 90)
                        .chunks(2)
                        .map(|c| {
                            let v = (i32::from(c[0]) << 8 | i32::from(c[1])) % 13261 - 2550;
                            if extreme {
                                if k % 3 == 0 { -2550 } else { 10710 }
                            } else {
                                v as i16
                            }
                        })
                        .collect()
                })
                .collect();
            let rows: [&[i16]; 6] = core::array::from_fn(|k| &sums[k][..]);
            let (mut o, mut o2) = (vec![7u8; n + 1], vec![7u8; n + 1]);
            assert_eq!(hpel_j(rows, &mut o[..n]), expected());
            hpel_j_scalar(rows, &mut o2[..n]);
            assert_eq!(o, o2, "j n {n}");
            assert_eq!(o[n], 7, "guard");
        }
    }
}

#[test]
fn avg16_matches_the_oracle() {
    for (sa, sb) in [(16, 16), (21, 40)] {
        let (a, b) = (bytes(15 * sa + 16, 3), bytes(15 * sb + 16, 4));
        let (mut o, mut o2) = ([0u8; 256], [0u8; 256]);
        assert_eq!(avg16(&a, sa, &b, sb, &mut o), expected());
        avg16_scalar(&a, sa, &b, sb, &mut o2);
        assert_eq!(o, o2);
    }
    let (z, f) = (vec![254u8; 256], vec![255u8; 256]);
    let mut o = [0u8; 256];
    avg16(&z, 16, &f, 16, &mut o);
    assert_eq!(o, [255; 256]);
}
