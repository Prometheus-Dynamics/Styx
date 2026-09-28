//! Every SIMD leaf, on every backend this machine has (x86: SSE2 only, SSE2 + SSSE3, then all
//! detected features; AArch64: NEON), and the public dispatchers, against the scalar oracle:
//! widths around each vector size, random and extreme data, guard values after each output
//! row (nothing may be written past it).

use super::*;

const GUARD: u16 = 0xA5A7;

/// Who runs a kernel.
#[derive(Clone, Copy, Debug)]
enum Runner {
    Oracle,
    Dispatch,
    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
    X86(X86FeatureSet),
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    Neon,
}

fn runners() -> Vec<Runner> {
    #[allow(unused_mut)]
    let mut out = vec![Runner::Dispatch];
    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        let f = X86FeatureSet::detect();
        let sse2 = X86FeatureSet {
            sse2: f.sse2,
            ..Default::default()
        };
        let ssse3 = X86FeatureSet {
            ssse3: f.ssse3,
            ..sse2
        };
        out.extend([sse2, ssse3, f].map(Runner::X86));
    }
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    out.push(Runner::Neon);
    out
}

/// A leaf's outcome as pixels done (`None`: nothing).
fn pixels(outcome: Option<(SimdBackend, usize)>) -> usize {
    outcome.map_or(0, |(_, n)| n)
}

/// Call leaf `$name` for a leaf runner; `Oracle` and `Dispatch` are the caller's business.
macro_rules! leaf_call {
    ($runner:expr, $name:ident($($arg:expr),* $(,)?)) => {
        match $runner {
            #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
            Runner::X86(f) => pixels(x86::$name(f, $($arg),*)),
            #[cfg(all(feature = "neon", target_arch = "aarch64"))]
            Runner::Neon => pixels(neon::$name($($arg),*)),
            Runner::Oracle | Runner::Dispatch => unreachable!(),
        }
    };
}

/// Deterministic random words below or at `max`; one seed in five gives only 0 and `max`.
fn words(len: usize, seed: u64, max: u16) -> Vec<u16> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let extreme = seed.is_multiple_of(5);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if extreme {
                if x & 1 == 0 { 0 } else { max }
            } else {
                (x % (max as u64 + 1)) as u16
            }
        })
        .collect()
}

fn bytes(len: usize, seed: u64) -> Vec<u8> {
    words(len, seed, 255).into_iter().map(|w| w as u8).collect()
}

fn widths() -> impl Iterator<Item = usize> {
    (0..=80).chain([95, 96, 97, 127, 128, 129, 255, 256, 257, 1280])
}

/// Run `run` for every runner and width: it gets fresh output planes of `units * width`
/// values plus guards and returns the pixels it did; outputs must match the oracle's.
fn check<T: Copy + PartialEq + std::fmt::Debug>(
    name: &str,
    planes: usize,
    units: usize,
    guard: T,
    run: impl Fn(Runner, usize, &mut [Vec<T>]) -> usize,
) {
    for width in widths() {
        let fresh = || vec![vec![guard; units * width + 40]; planes];
        let mut want = fresh();
        run(Runner::Oracle, width, &mut want);
        for runner in runners() {
            let mut got = fresh();
            let done = run(runner, width, &mut got);
            assert!(done <= width, "{name} {runner:?} width {width}: did {done}");
            if matches!(runner, Runner::Dispatch) {
                assert_eq!(done, width, "{name}: dispatcher must finish the row");
            }
            for (p, (g, w)) in got.iter().zip(&want).enumerate() {
                assert_eq!(
                    g[..done * units],
                    w[..done * units],
                    "{name} {runner:?} width {width} plane {p}"
                );
                assert!(
                    g[units * width..].iter().all(|&v| v == guard),
                    "{name} {runner:?} width {width} plane {p}: wrote past the row"
                );
            }
        }
    }
}

fn split3<T>(o: &mut [Vec<T>]) -> [&mut [T]; 3] {
    let [a, b, c] = o else { panic!("three planes") };
    [a.as_mut_slice(), b.as_mut_slice(), c.as_mut_slice()]
}

#[test]
fn unpack_matches() {
    for raw12 in [false, true] {
        check("unpack", 1, 1, GUARD, |runner, w, out| {
            let len = if raw12 {
                raw12_bytes(w)
            } else {
                raw10_bytes(w)
            };
            let src = bytes(len, w as u64);
            let dst = out[0].as_mut_slice();
            match (runner, raw12) {
                (Runner::Oracle, false) => scalar::unpack_raw10_row(&src, dst, w),
                (Runner::Oracle, true) => scalar::unpack_raw12_row(&src, dst, w),
                (Runner::Dispatch, false) => drop(unpack_raw10_row(&src, dst, w)),
                (Runner::Dispatch, true) => drop(unpack_raw12_row(&src, dst, w)),
                (_, false) => return leaf_call!(runner, unpack_raw10_row(&src, dst, w)),
                (_, true) => return leaf_call!(runner, unpack_raw12_row(&src, dst, w)),
            }
            w
        });
    }
}

#[test]
fn raw10_unpacks_csi2_bit_order() {
    // Pixels 0x3FF, 0x001, 0x2AA, 0x155: high bytes then the low bits of each, pixel 0 lowest.
    let src = [0xFF, 0x00, 0xAA, 0x55, 0b01_10_01_11];
    let mut dst = [0u16; 4];
    unpack_raw10_row(&src, &mut dst, 4);
    assert_eq!(dst, [0x3FF, 0x001, 0x2AA, 0x155]);
    let src = [0xAB, 0xCD, 0x21];
    let mut dst = [0u16; 2];
    unpack_raw12_row(&src, &mut dst, 2);
    assert_eq!(dst, [0xAB1, 0xCD2]);
}

#[test]
fn front_matches() {
    for bits in [8u32, 10, 12, 16] {
        check("front", 1, 1, GUARD, |runner, w, out| {
            let max = ((1u32 << bits) - 1) as u16;
            let row = &mut out[0];
            row[..w].copy_from_slice(&words(w, w as u64 + 7, max));
            let gains = words(w, w as u64 + 3, u16::MAX);
            let black = [(w as u16 * 13) & (max / 8), (w as u16 * 7) & (max / 8)];
            let shift = 16 - bits;
            match runner {
                Runner::Oracle => scalar::front_row(row, black, &gains, shift, w),
                Runner::Dispatch => drop(front_row(row, black, &gains, shift, w)),
                _ => return leaf_call!(runner, front_row(&mut row[..w], black, &gains, shift, w)),
            }
            w
        });
    }
}

const KINDS: [RowKind; 4] = [
    RowKind {
        green_even: false,
        x_is_red: false,
    },
    RowKind {
        green_even: false,
        x_is_red: true,
    },
    RowKind {
        green_even: true,
        x_is_red: false,
    },
    RowKind {
        green_even: true,
        x_is_red: true,
    },
];

#[test]
fn demosaic_bilinear_matches() {
    for kind in KINDS {
        check("bilinear", 3, 1, GUARD, |runner, w, out| {
            let rows: Vec<Vec<u16>> = (0..3)
                .map(|i| words(w + 2, w as u64 * 3 + i, 4095))
                .collect();
            let rows = [&rows[0][..], &rows[1][..], &rows[2][..]];
            let out = split3(out);
            match runner {
                Runner::Oracle => scalar::demosaic_bilinear_row(rows, out, w, kind),
                Runner::Dispatch => drop(demosaic_bilinear_row(rows, out, w, kind)),
                _ => {
                    let [r, g, b] = out;
                    return leaf_call!(
                        runner,
                        demosaic_bilinear_row(
                            rows,
                            [&mut r[..w], &mut g[..w], &mut b[..w]],
                            w,
                            kind
                        )
                    );
                }
            }
            w
        });
    }
}

#[test]
fn demosaic_mhc_matches() {
    for kind in KINDS {
        check("mhc", 3, 1, GUARD, |runner, w, out| {
            let rows: Vec<Vec<u16>> = (0..5)
                .map(|i| words(w + 4, w as u64 * 5 + i, 4095))
                .collect();
            let rows = [
                &rows[0][..],
                &rows[1][..],
                &rows[2][..],
                &rows[3][..],
                &rows[4][..],
            ];
            let out = split3(out);
            match runner {
                Runner::Oracle => scalar::demosaic_mhc_row(rows, out, w, kind),
                Runner::Dispatch => drop(demosaic_mhc_row(rows, out, w, kind)),
                _ => {
                    let [r, g, b] = out;
                    return leaf_call!(
                        runner,
                        demosaic_mhc_row(rows, [&mut r[..w], &mut g[..w], &mut b[..w]], w, kind)
                    );
                }
            }
            w
        });
    }
}

#[test]
fn bayer_luma_matches() {
    check("bayer_luma", 1, 1, GUARD, |runner, w, out| {
        let rows: Vec<Vec<u16>> = (0..3)
            .map(|i| words(w + 2, w as u64 * 3 + i, 4095))
            .collect();
        let rows = [&rows[0][..], &rows[1][..], &rows[2][..]];
        let dst = out[0].as_mut_slice();
        match runner {
            Runner::Oracle => scalar::bayer_luma_row(rows, dst, w),
            Runner::Dispatch => drop(bayer_luma_row(rows, dst, w)),
            _ => return leaf_call!(runner, bayer_luma_row(rows, &mut dst[..w], w)),
        }
        w
    });
}

const PATTERNS: [CfaPattern; 4] = [
    CfaPattern::Rggb,
    CfaPattern::Bggr,
    CfaPattern::Grbg,
    CfaPattern::Gbrg,
];

#[test]
fn quads_match() {
    for pattern in PATTERNS {
        check("quad_rgb", 3, 1, GUARD, |runner, w, out| {
            let (t, b) = (
                words(2 * w, w as u64, 4095),
                words(2 * w, w as u64 + 1, 4095),
            );
            let out = split3(out);
            match runner {
                Runner::Oracle => scalar::quad_rgb_row(&t, &b, out, w, pattern),
                Runner::Dispatch => drop(quad_rgb_row(&t, &b, out, w, pattern)),
                _ => {
                    let [r, g, bl] = out;
                    return leaf_call!(
                        runner,
                        quad_rgb_row(&t, &b, [&mut r[..w], &mut g[..w], &mut bl[..w]], w, pattern)
                    );
                }
            }
            w
        });
    }
    check("quad_luma", 1, 1, GUARD, |runner, w, out| {
        let (t, b) = (
            words(2 * w, w as u64, 4095),
            words(2 * w, w as u64 + 1, 4095),
        );
        let dst = out[0].as_mut_slice();
        match runner {
            Runner::Oracle => scalar::quad_luma_row(&t, &b, dst, w),
            Runner::Dispatch => drop(quad_luma_row(&t, &b, dst, w)),
            _ => return leaf_call!(runner, quad_luma_row(&t, &b, &mut dst[..w], w)),
        }
        w
    });
}

#[test]
fn ccm_matches() {
    let matrices: [[i16; 9]; 3] = [
        [1024, 0, 0, 0, 1024, 0, 0, 0, 1024],
        [1843, -614, -205, -307, 1536, -205, -102, -717, 1843],
        [
            32767, -32768, 32767, -32768, 32767, -32768, 100, -5000, 9000,
        ],
    ];
    for m in matrices {
        check("ccm", 3, 1, GUARD, |runner, w, out| {
            for (i, plane) in out.iter_mut().enumerate() {
                plane[..w].copy_from_slice(&words(w, w as u64 * 3 + i as u64, 4095));
            }
            let planes = split3(out);
            match runner {
                Runner::Oracle => scalar::ccm_row(planes, &m, w),
                Runner::Dispatch => drop(ccm_row(planes, &m, w)),
                _ => {
                    let [r, g, b] = planes;
                    return leaf_call!(
                        runner,
                        ccm_row([&mut r[..w], &mut g[..w], &mut b[..w]], &m, w)
                    );
                }
            }
            w
        });
    }
}

#[test]
fn narrow_and_lut_match() {
    check("narrow", 1, 1, 0xA7u8, |runner, w, out| {
        // Include out-of-range inputs: narrowing saturates.
        let src = words(w, w as u64, if w % 2 == 0 { 4095 } else { u16::MAX });
        let dst = out[0].as_mut_slice();
        match runner {
            Runner::Oracle => scalar::narrow_row(&src, dst, w),
            Runner::Dispatch => drop(narrow_row(&src, dst, w)),
            _ => return leaf_call!(runner, narrow_row(&src, &mut dst[..w], w)),
        }
        w
    });
    let lut: [u8; 4096] = std::array::from_fn(|i| (i * 7 % 256) as u8);
    let src = [0u16, 4095, 5000, 17];
    let mut dst = [0u8; 4];
    lut_row(&src, &mut dst, &lut, 4);
    assert_eq!(dst, [lut[0], lut[4095], lut[4095], lut[17]]);
}

#[test]
fn interleave_and_luma_match() {
    check("interleave", 1, 3, 0xA7u8, |runner, w, out| {
        let p: Vec<Vec<u8>> = (0..3).map(|i| bytes(w, w as u64 * 3 + i)).collect();
        let planes = [&p[0][..], &p[1][..], &p[2][..]];
        let dst = out[0].as_mut_slice();
        match runner {
            Runner::Oracle => scalar::interleave_rgb_row(planes, dst, w),
            Runner::Dispatch => drop(interleave_rgb_row(planes, dst, w)),
            _ => return leaf_call!(runner, interleave_rgb_row(planes, &mut dst[..3 * w], w)),
        }
        w
    });
    for c in [YuvCoeffs::BT601_FULL, YuvCoeffs::BT709_LIMITED] {
        check("rgb_to_y", 1, 1, 0xA7u8, |runner, w, out| {
            let p: Vec<Vec<u8>> = (0..3).map(|i| bytes(w, w as u64 * 3 + i)).collect();
            let planes = [&p[0][..], &p[1][..], &p[2][..]];
            let dst = out[0].as_mut_slice();
            match runner {
                Runner::Oracle => scalar::rgb_to_y_row(planes, dst, w, &c),
                Runner::Dispatch => drop(rgb_to_y_row(planes, dst, w, &c)),
                _ => return leaf_call!(runner, rgb_to_y_row(planes, &mut dst[..w], w, &c)),
            }
            w
        });
    }
}

#[test]
fn chroma_matches() {
    for c in [YuvCoeffs::BT601_FULL, YuvCoeffs::BT709_LIMITED] {
        for interleaved in [false, true] {
            // Interleaved: one plane of UV pairs; planar: U and V.
            let (planes, units) = if interleaved { (1, 2) } else { (2, 1) };
            check("rgb_to_uv", planes, units, 0xA7u8, |runner, w, out| {
                let rows: Vec<Vec<u8>> = (0..6).map(|i| bytes(2 * w, w as u64 * 6 + i)).collect();
                let top = [&rows[0][..], &rows[1][..], &rows[2][..]];
                let bottom = [&rows[3][..], &rows[4][..], &rows[5][..]];
                let mut empty = Vec::new();
                let (u, v) = match out {
                    [u, v] => (u, v),
                    [u] => (u, &mut empty),
                    _ => unreachable!(),
                };
                match runner {
                    Runner::Oracle => scalar::rgb_to_uv_row(top, bottom, u, v, w, &c, interleaved),
                    Runner::Dispatch => drop(rgb_to_uv_row(top, bottom, u, v, w, &c, interleaved)),
                    _ => {
                        let (u, v) = if interleaved {
                            (&mut u[..2 * w], &mut v[..0])
                        } else {
                            (&mut u[..w], &mut v[..w])
                        };
                        return leaf_call!(
                            runner,
                            rgb_to_uv_row(top, bottom, u, v, w, &c, interleaved)
                        );
                    }
                }
                w
            });
        }
    }
}

#[test]
fn row_kinds_follow_the_pattern() {
    for pattern in PATTERNS {
        for y in 0..4 {
            let kind = RowKind::of(pattern, y);
            for x in 0..4 {
                let ch = pattern.channel_at(x, y);
                assert_eq!(kind.is_green(x), ch == crate::format::Channel::Green);
                if ch != crate::format::Channel::Green {
                    assert_eq!(kind.x_is_red, ch == crate::format::Channel::Red);
                }
            }
        }
    }
}
