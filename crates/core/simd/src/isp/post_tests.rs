//! The output stages' kernels on every leaf against the oracle. The filters' `width` here is the
//! interior: rows hold `width + 2` samples, and the public dispatchers run whole rows, their
//! edge columns checked against the oracle's too.

use super::post::{self, RCP, Sharpening, scalar};
#[allow(unused_imports)]
use super::tests::pixels;
use super::tests::{Runner, bytes, check, leaf_call};
use super::*;

/// Three rows of `w` samples: random, or within a few codes of each other so filters take
/// their neighbours.
fn rows(w: usize, seed: u64) -> [Vec<u8>; 3] {
    [0, 1, 2].map(|i| {
        let mut r = bytes(w, seed * 3 + i);
        if seed % 2 == 1 {
            for v in &mut r {
                *v = 100 + *v % 12;
            }
        }
        r
    })
}

fn view(r: &[Vec<u8>; 3]) -> [&[u8]; 3] {
    [&r[0], &r[1], &r[2]]
}

/// Check a filter: `oracle` and `whole` over whole rows, `leaf` over the interior.
fn check_filter(
    name: &str,
    oracle: impl Fn([&[u8]; 3], &mut [u8]),
    whole: impl Fn([&[u8]; 3], &mut [u8]),
    leaf: impl Fn(Runner, [&[u8]; 3], &mut [u8]) -> usize,
) {
    for seed in [0u64, 1, 5, 6] {
        check(name, 1, 1, 0xA7u8, |runner, n, out| {
            let w = n + 2;
            let r = rows(w, seed + n as u64 * 7);
            let mut want = vec![0u8; w];
            oracle(view(&r), &mut want);
            match runner {
                Runner::Oracle => {
                    out[0][..n].copy_from_slice(&want[1..w - 1]);
                    n
                }
                Runner::Dispatch => {
                    let mut got = vec![0u8; w];
                    whole(view(&r), &mut got);
                    assert_eq!(got, want, "{name} dispatcher, width {w}");
                    out[0][..n].copy_from_slice(&got[1..w - 1]);
                    n
                }
                _ => leaf(runner, view(&r), &mut out[0][..n]),
            }
        });
    }
    // Rows too short to have an interior.
    for w in 0..3 {
        let r = rows(w, 2);
        let (mut got, mut want) = (vec![0u8; w], vec![0u8; w]);
        oracle(view(&r), &mut want);
        whole(view(&r), &mut got);
        assert_eq!(got, want, "{name}, width {w}");
    }
}

#[test]
fn rgb_to_planes_matches() {
    check("rgb_to_planes", 3, 1, 0xA7u8, |runner, w, out| {
        let rgb = bytes(3 * w, w as u64 * 5);
        let [y, cb, cr] = super::tests::split3(out);
        match runner {
            Runner::Oracle => scalar::rgb_to_planes_row(&rgb, y, cb, cr, w),
            Runner::Dispatch => _ = post::rgb_to_planes_row(&rgb, y, cb, cr, w),
            _ => {
                return leaf_call!(
                    runner,
                    rgb_to_planes_row(&rgb, &mut y[..w], &mut cb[..w], &mut cr[..w], w)
                );
            }
        }
        w
    });
}

#[test]
fn rgb_to_planes_extremes_are_exact() {
    // Every corner of the RGB cube, where the chroma sum reaches its 16-bit limit.
    let mut rgb = Vec::new();
    for _ in 0..16 {
        for v in 0..8u8 {
            rgb.extend([0, 1, 2].map(|k| if v >> k & 1 == 1 { 255 } else { 0 }));
        }
    }
    let w = rgb.len() / 3;
    let mut want = [vec![0u8; w], vec![0u8; w], vec![0u8; w]];
    let [a, b, c] = &mut want;
    scalar::rgb_to_planes_row(&rgb, a, b, c, w);
    let mut got = [vec![0u8; w], vec![0u8; w], vec![0u8; w]];
    let [a, b, c] = &mut got;
    post::rgb_to_planes_row(&rgb, a, b, c, w);
    assert_eq!(got, want);
}

#[test]
fn planes_to_rgb_matches() {
    check("planes_to_rgb", 3, 1, 0xA7u8, |runner, w, out| {
        let p: Vec<Vec<u8>> = (0..3).map(|i| bytes(w, w as u64 * 9 + i)).collect();
        let planes = [&p[0][..], &p[1][..], &p[2][..]];
        let [r, g, b] = super::tests::split3(out);
        match runner {
            Runner::Oracle => scalar::planes_to_rgb_row(planes, [r, g, b], w),
            Runner::Dispatch => _ = post::planes_to_rgb_row(planes, [r, g, b], w),
            _ => {
                return leaf_call!(
                    runner,
                    planes_to_rgb_row(planes, [&mut r[..w], &mut g[..w], &mut b[..w]], w)
                );
            }
        }
        w
    });
}

#[test]
fn every_count_of_the_sigma_filter_rounds_as_the_oracle() {
    // `n = 1` holds 65535 for 65536: still the centre itself.
    for c in 0..=255u32 {
        assert_eq!((c * u32::from(RCP[1]) + (1 << 15)) >> 16, c);
    }
}

#[test]
fn sigma_matches() {
    let tables: [[u16; 256]; 3] = [
        [1020; 256],
        [0; 256],
        core::array::from_fn(|c| (c as u16 / 4 + 3).min(1020)),
    ];
    for table in &tables {
        for s in [0i16, 1, 77, 128, 255, 256] {
            check_filter(
                "sigma",
                |r, d| scalar::sigma_row(r, d, table, s),
                |r, d| _ = post::sigma_row(r, d, table, s),
                |runner, r, d| leaf_call!(runner, sigma_row(r, d, table, s)),
            );
        }
    }
}

#[test]
fn chroma_matches() {
    for t in [0i16, 3, 40, 1020] {
        for s in [0i16, 1, 77, 128, 255, 256] {
            check_filter(
                "chroma",
                |r, d| scalar::chroma_row(r, d, t, s),
                |r, d| _ = post::chroma_row(r, d, t, s),
                |runner, r, d| leaf_call!(runner, chroma_row(r, d, t, s)),
            );
        }
    }
}

#[test]
fn sharpen_matches() {
    for (amount, threshold, limit) in [
        (0, 0, 0),
        (256, 0, 30),
        (128, 4, 6),
        (512, 0, 1000),
        (32767, 0, 255),
        (32767, 300, 255),
        (32768, 0, 255),
        (1_000_000, 2, 9),
    ] {
        let p = Sharpening {
            amount,
            threshold,
            limit,
        };
        check_filter(
            "sharpen",
            |r, d| scalar::sharpen_row(r, d, p),
            |r, d| _ = post::sharpen_row(r, d, p),
            |runner, r, d| {
                // As the dispatcher: past 16 bits stays scalar.
                if amount > i32::from(i16::MAX) {
                    return 0;
                }
                let (thr, lim) = (threshold.min(256) as i16, limit.min(255) as i16);
                leaf_call!(runner, sharpen_row(r, d, amount as i16, thr, lim))
            },
        );
    }
}
