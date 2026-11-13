//! Every SIMD leaf, on every backend this machine has (x86: SSE2 only, SSE2 + SSSE3, then all
//! detected features; AArch64: NEON), and the public dispatchers, against the scalar oracle:
//! widths around each vector size, random and extreme data, guard values after each output
//! row (nothing may be written past it).

use super::*;

const GUARD: u16 = 0xA5A7;

/// The sRGB transfer curve on 0..1, as the soft ISP's `ToneCurve::Srgb` evaluates it.
pub(super) fn srgb(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Who runs a kernel.
#[derive(Clone, Copy, Debug)]
pub(super) enum Runner {
    Oracle,
    Dispatch,
    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
    X86(X86FeatureSet),
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    Neon,
    #[cfg(all(
        feature = "neon",
        feature = "std",
        target_arch = "arm",
        target_os = "linux"
    ))]
    Neon32,
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    Rvv,
}

pub(super) fn runners() -> Vec<Runner> {
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
        // AVX2 alone too where AVX-512 VBMI would take its place.
        let avx2 = X86FeatureSet {
            avx512vbmi: false,
            ..f
        };
        out.extend([sse2, ssse3, avx2, f].map(Runner::X86));
    }
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    out.push(Runner::Neon);
    #[cfg(all(
        feature = "neon",
        feature = "std",
        target_arch = "arm",
        target_os = "linux"
    ))]
    {
        assert!(crate::has_neon32(), "the armv7 gate runs QEMU's Cortex-A7");
        out.push(Runner::Neon32);
    }
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    {
        assert!(crate::has_rvv(), "the riscv gate runs QEMU with v=true");
        out.push(Runner::Rvv);
    }
    out
}

/// A leaf's outcome as pixels done (`None`: nothing).
pub(super) fn pixels(outcome: Option<(SimdBackend, usize)>) -> usize {
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
            #[cfg(all(feature = "neon", feature = "std", target_arch = "arm", target_os = "linux"))]
            Runner::Neon32 => pixels(neon32::$name($($arg),*)),
            #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
            Runner::Rvv => pixels(rvv::$name($($arg),*)),
            Runner::Oracle | Runner::Dispatch => unreachable!(),
        }
    };
}

pub(super) use leaf_call;

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

pub(super) fn bytes(len: usize, seed: u64) -> Vec<u8> {
    words(len, seed, 255).into_iter().map(|w| w as u8).collect()
}

fn widths() -> impl Iterator<Item = usize> {
    (0..=80).chain([95, 96, 97, 127, 128, 129, 255, 256, 257, 1280])
}

/// Run `run` for every runner and width: it gets fresh output planes of `units * width`
/// values plus guards and returns the pixels it did; outputs must match the oracle's.
pub(super) fn check<T: Copy + PartialEq + std::fmt::Debug>(
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

pub(super) fn split3<T>(o: &mut [Vec<T>]) -> [&mut [T]; 3] {
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
fn demosaic_edge_matches() {
    for kind in KINDS {
        check("edge", 3, 1, GUARD, |runner, w, out| {
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
                Runner::Oracle => scalar::demosaic_edge_row(rows, out, w, kind),
                Runner::Dispatch => drop(demosaic_edge_row(rows, out, w, kind)),
                _ => {
                    let [r, g, b] = out;
                    return leaf_call!(
                        runner,
                        demosaic_edge_row(rows, [&mut r[..w], &mut g[..w], &mut b[..w]], w, kind)
                    );
                }
            }
            w
        });
    }
}

#[test]
fn demosaic_edge_matches_at_the_extremes() {
    for kind in KINDS {
        check("edge extremes", 3, 1, GUARD, |runner, w, out| {
            let rows: Vec<Vec<u16>> = (0..5)
                .map(|i| {
                    words(w + 4, w as u64 * 5 + i, 1)
                        .into_iter()
                        .map(|v| v * 4095)
                        .collect()
                })
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
                Runner::Oracle => scalar::demosaic_edge_row(rows, out, w, kind),
                Runner::Dispatch => drop(demosaic_edge_row(rows, out, w, kind)),
                _ => {
                    let [r, g, b] = out;
                    return leaf_call!(
                        runner,
                        demosaic_edge_row(rows, [&mut r[..w], &mut g[..w], &mut b[..w]], w, kind)
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

/// The byte-source luma kernels equal the 12-bit kernels on the same samples times 16 (the front
/// end's unpacked value of an 8-bit sample with unit gain), with the frame's mirrored edges.
#[test]
fn u8_luma_matches_the_12_bit_route() {
    for width in (4..=40).chain([95, 96, 97, 255, 256, 1280]) {
        for seed in 0..6u64 {
            let frame: Vec<Vec<u8>> = (0..3)
                .map(|i| bytes(width, seed * 7 + i + width as u64))
                .collect();
            // Output columns: the whole row, or an inner window of it (its own origin).
            for (x0, n) in [(0, width), (2.min(width - 2), width - 2)] {
                let n = n.min(width - x0);
                // The 12-bit rows: 16 x the bytes at x0 - 1 ..= x0 + n, mirrored.
                let mirror = |i: isize| -> usize {
                    let w = width as isize;
                    let r = if i < 0 {
                        -i
                    } else if i >= w {
                        2 * w - 2 - i
                    } else {
                        i
                    };
                    r.clamp(0, w - 1) as usize
                };
                let wide: Vec<Vec<u16>> = frame
                    .iter()
                    .map(|row| {
                        (0..n + 2)
                            .map(|j| 16 * row[mirror(x0 as isize - 1 + j as isize)] as u16)
                            .collect()
                    })
                    .collect();
                let mut want = vec![0u16; n];
                scalar::bayer_luma_row([&wide[0], &wide[1], &wide[2]], &mut want, n);
                let mut got = vec![0u16; n];
                bayer_luma_u8_row([&frame[0], &frame[1], &frame[2]], x0, &mut got, n, width);
                assert_eq!(got, want, "bayer width {width} x0 {x0} seed {seed}");
            }
            // Quads: 2 n bytes of two rows from column 0.
            let half = width / 2;
            let pairs: Vec<u16> = (0..2 * half).map(|j| 16 * frame[0][j] as u16).collect();
            let pairs_b: Vec<u16> = (0..2 * half).map(|j| 16 * frame[1][j] as u16).collect();
            let mut want = vec![0u16; half];
            scalar::quad_luma_row(&pairs, &pairs_b, &mut want, half);
            let mut got = vec![0u16; half];
            quad_luma_u8_row(&frame[0][..2 * half], &frame[1][..2 * half], &mut got, half);
            assert_eq!(got, want, "quads width {width} seed {seed}");
        }
    }
}

/// Each byte-luma leaf on its own (every feature set this machine has, and NEON on AArch64),
/// finished by the scalar kernel as the dispatcher does, equals the scalar oracle.
#[test]
fn u8_luma_leaves_match() {
    for width in (8..=40).chain([95, 96, 97, 255, 1280]) {
        for seed in 0..4u64 {
            let frame: Vec<Vec<u8>> = (0..3)
                .map(|i| bytes(width, seed * 11 + i + width as u64))
                .collect();
            let rows = [&frame[0][..], &frame[1][..], &frame[2][..]];
            // Interior columns only (the dispatcher does the mirrored edges).
            let base = 1;
            let m = width - 2;
            let mut want = vec![0u16; m];
            scalar::bayer_luma_u8_interior(rows, base, &mut want);
            let mut leaves: Vec<(&str, usize, Vec<u16>)> = Vec::new();
            #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
            for f in [
                X86FeatureSet {
                    sse2: true,
                    ..Default::default()
                },
                X86FeatureSet::detect(),
            ] {
                if !f.sse2 {
                    continue;
                }
                let mut got = vec![0u16; m];
                let outcome = x86::bayer_luma_u8_row(f, rows, base, &mut got);
                let n = outcome.map_or(0, |(_, n)| n);
                scalar::bayer_luma_u8_interior(rows, base + n, &mut got[n..]);
                leaves.push(("x86", n, got));
            }
            #[cfg(all(feature = "neon", target_arch = "aarch64"))]
            {
                let mut got = vec![0u16; m];
                let n = neon::bayer_luma_u8_row(rows, base, &mut got).map_or(0, |(_, n)| n);
                scalar::bayer_luma_u8_interior(rows, base + n, &mut got[n..]);
                leaves.push(("neon", n, got));
            }
            for (name, _, got) in leaves {
                assert_eq!(got, want, "{name} width {width} seed {seed}");
            }
        }
    }
}

/// The quad byte-luma leaves on their own (each feature set, NEON on AArch64) against the
/// scalar kernel.
#[test]
fn u8_quad_leaves_match() {
    for width in (1..=40).chain([63, 64, 65, 255, 640]) {
        for seed in 0..4u64 {
            let top = bytes(2 * width, seed * 5 + width as u64);
            let bottom = bytes(2 * width, seed * 5 + 1 + width as u64);
            let mut want = vec![0u16; width];
            scalar::quad_luma_u8_row(&top, &bottom, &mut want, width);
            #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
            for f in [
                X86FeatureSet {
                    sse2: true,
                    ..Default::default()
                },
                X86FeatureSet::detect(),
            ] {
                if !f.sse2 {
                    continue;
                }
                let mut got = vec![0u16; width];
                let n =
                    x86::quad_luma_u8_row(f, &top, &bottom, &mut got, width).map_or(0, |(_, n)| n);
                scalar::quad_luma_u8_row(&top[2 * n..], &bottom[2 * n..], &mut got[n..], width - n);
                assert_eq!(got, want, "x86 width {width} seed {seed}");
            }
            #[cfg(all(feature = "neon", target_arch = "aarch64"))]
            {
                let mut got = vec![0u16; width];
                let n =
                    neon::quad_luma_u8_row(&top, &bottom, &mut got, width).map_or(0, |(_, n)| n);
                scalar::quad_luma_u8_row(&top[2 * n..], &bottom[2 * n..], &mut got[n..], width - n);
                assert_eq!(got, want, "neon width {width} seed {seed}");
            }
        }
    }
}

/// The fused tone-in-sum kernels equal the luma kernel followed by the table, for random tables
/// and rows, and for every byte value as a uniform neighbourhood (the sum is then `16 b`).
#[test]
fn u8_tone_matches_luma_then_table() {
    let table: [u8; 4096] =
        core::array::from_fn(|k| (k as u32).wrapping_mul(2_654_435_761).rotate_left(7) as u8);
    for width in (4..=40).chain([95, 96, 97, 255, 1280]) {
        for seed in 0..4u64 {
            let frame: Vec<Vec<u8>> = (0..3)
                .map(|i| bytes(width, seed * 13 + i + width as u64))
                .collect();
            let rows = [&frame[0][..], &frame[1][..], &frame[2][..]];
            for (x0, n) in [(0, width), (2.min(width - 2), width - 2)] {
                let n = n.min(width - x0);
                let mut luma = vec![0u16; n];
                bayer_luma_u8_row(rows, x0, &mut luma, n, width);
                let want: Vec<u8> = luma.iter().map(|&v| table[v as usize]).collect();
                let mut got = vec![0u8; n];
                bayer_tone_u8_row(rows, x0, &mut got, n, width, &table);
                assert_eq!(got, want, "bayer width {width} x0 {x0} seed {seed}");
            }
            let half = width / 2;
            let mut luma = vec![0u16; half];
            quad_luma_u8_row(
                &frame[0][..2 * half],
                &frame[1][..2 * half],
                &mut luma,
                half,
            );
            let want: Vec<u8> = luma.iter().map(|&v| table[v as usize]).collect();
            let mut got = vec![0u8; half];
            quad_tone_u8_row(
                &frame[0][..2 * half],
                &frame[1][..2 * half],
                &mut got,
                half,
                &table,
            );
            assert_eq!(got, want, "quads width {width} seed {seed}");
        }
    }
    // Uniform neighbourhoods: a byte b in every neighbour makes the bayer sum 16 b and the quad
    // sum 4 b (times four: 16 b), so the table entry is at 16 b.
    for b in 0..=255u8 {
        let row = [b; 9];
        let rows = [&row[..], &row[..], &row[..]];
        let mut got = [0u8; 1];
        bayer_tone_u8_row(rows, 1, &mut got, 1, 9, &table);
        assert_eq!(got[0], table[16 * b as usize], "uniform byte {b}");
        let mut got = [0u8; 1];
        quad_tone_u8_row(&row[..4], &row[..4], &mut got, 1, &table);
        assert_eq!(got[0], table[16 * b as usize], "uniform quad byte {b}");
    }
}

const PATTERNS: [CfaOrder; 4] = [
    CfaOrder::Rggb,
    CfaOrder::Bggr,
    CfaOrder::Grbg,
    CfaOrder::Gbrg,
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
    for seed in 0..3u64 {
        let nodes: [u8; 257] = std::array::from_fn(|i| bytes(257, seed)[i]);
        let lut = ToneLut::from_nodes(nodes);
        check("lut", 1, 1, 0xA7u8, |runner, w, out| {
            let src = words(w, w as u64 + seed, if w % 3 == 0 { u16::MAX } else { 4095 });
            let dst = out[0].as_mut_slice();
            match runner {
                Runner::Oracle => scalar::lut_row(&src, dst, lut.full(), w),
                Runner::Dispatch => drop(lut_row(&src, dst, &lut, w)),
                _ => return leaf_call!(runner, lut_row(&src, &mut dst[..w], &lut, w)),
            }
            w
        });
    }
    // The expanded table interpolates the nodes.
    let lut = ToneLut::from_nodes(std::array::from_fn(|i| (i as u32 * 255 / 256) as u8));
    assert_eq!(lut.full()[0], 0);
    assert_eq!(lut.full()[16 * 100], lut.nodes()[100]);
    assert_eq!(
        lut.full()[16 * 100 + 8],
        (lut.nodes()[100] as u32 + lut.nodes()[101] as u32).div_ceil(2) as u8
    );
}

/// Every input code (and some above 4095) through every node set shape: random, extreme
/// steps (0 next to 255 both ways), falling, flat.
#[test]
fn lut_every_code() {
    let (src, shapes) = lut_cases();
    for nodes in shapes {
        let lut = ToneLut::from_nodes(nodes);
        let mut want = vec![0u8; src.len()];
        scalar::lut_row(&src, &mut want, lut.full(), src.len());
        for runner in runners() {
            let mut got = vec![0u8; src.len()];
            let done = match runner {
                Runner::Oracle => unreachable!(),
                Runner::Dispatch => {
                    let _ = lut_row(&src, &mut got, &lut, src.len());
                    src.len()
                }
                _ => leaf_call!(runner, lut_row(&src, &mut got, &lut, src.len())),
            };
            assert_eq!(got[..done], want[..done], "{runner:?}");
        }
    }
}

/// Every input code (and some above 4095), and the node sets of [`lut_every_code`].
fn lut_cases() -> (Vec<u16>, [[u8; 257]; 5]) {
    let src = (0..4096 + 64)
        .map(|x| if x < 4096 { x } else { u16::MAX - x })
        .collect();
    let shapes = [
        std::array::from_fn(|i| bytes(257, 7)[i]),
        std::array::from_fn(|i| if i % 2 == 0 { 0 } else { 255 }),
        std::array::from_fn(|i| 255 - (i as u32 * 255 / 256) as u8),
        [200; 257],
        std::array::from_fn(|i| if i < 128 { 0 } else { 255 }),
    ];
    (src, shapes)
}

/// The AVX-512 VBMI tone table where the CPU has it, saying which happened: CI's log line
/// ("ran on AVX-512 VBMI" or "skipped: no AVX-512 here") records whether a run exercised it.
#[test]
fn avx512vbmi_tone_leaf() {
    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        let f = X86FeatureSet::detect();
        if !f.avx512vbmi {
            eprintln!("skipped: no AVX-512 here");
            return;
        }
        let (src, shapes) = lut_cases();
        for nodes in shapes {
            let lut = ToneLut::from_nodes(nodes);
            let mut want = vec![0u8; src.len()];
            scalar::lut_row(&src, &mut want, lut.full(), src.len());
            let mut got = vec![0u8; src.len()];
            let (backend, done) = x86::lut_row(f, &src, &mut got, &lut, src.len()).unwrap();
            assert_eq!(backend, SimdBackend::X86Avx512Vbmi);
            assert_eq!(got[..done], want[..done]);
        }
        eprintln!("ran on AVX-512 VBMI");
    }
    #[cfg(not(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64"))))]
    eprintln!("skipped: no AVX-512 here");
}

/// The quadratic tone curve's leaves against its scalar evaluation, bit for bit: a fitted
/// curve and coefficients that saturate (the sums and the output, both ways).
#[test]
fn poly_tone_matches() {
    let srgb = ToneLut::from_curve(srgb);
    let polys = [
        poly::PolyTone::fit(srgb.full()).expect("sRGB fits"),
        poly::PolyTone {
            c: [
                [
                    [-900, 30000, 8, -32768, 32767, 3, 8000, 0],
                    [5, -7, 9000, 1, 2, -3, 0, 0],
                ],
                [
                    [12000, -400, 32767, 0, -32768, 7000, -3, 5],
                    [-32768, 1, 4, 0, 30000, 7, 0, 0],
                ],
                [
                    [-30000, 20000, 0, 1, 32767, -2000, 15, 0],
                    [9, 32767, -5, 6, 0, -20000, 0, 0],
                ],
            ],
        },
    ];
    for (k, p) in polys.iter().enumerate() {
        check("poly", 1, 1, 0xA7u8, |runner, w, out| {
            let src = words(
                w,
                w as u64 + k as u64,
                if w % 3 == 0 { u16::MAX } else { 4095 },
            );
            let dst = out[0].as_mut_slice();
            match runner {
                Runner::Oracle => poly::poly_row(&src, dst, p, w),
                Runner::Dispatch => drop(poly_row(&src, dst, p, w)),
                _ => return leaf_call!(runner, poly_row(&src, &mut dst[..w], p, w)),
            }
            w
        });
    }
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

/// The colour at column `x`, row `y` of a mosaic with `p` (as the soft ISP's `CfaPattern`).
fn channel_at(p: CfaOrder, x: usize, y: usize) -> styx_format::Channel {
    use styx_format::Channel::{Blue, Green, Red};
    let quad = match p {
        CfaOrder::Rggb => [Red, Green, Green, Blue],
        CfaOrder::Bggr => [Blue, Green, Green, Red],
        CfaOrder::Grbg => [Green, Red, Blue, Green],
        CfaOrder::Gbrg => [Green, Blue, Red, Green],
    };
    quad[(y & 1) * 2 + (x & 1)]
}

#[test]
fn row_kinds_follow_the_pattern() {
    for pattern in PATTERNS {
        for y in 0..4 {
            let kind = RowKind::of(pattern, y);
            for x in 0..4 {
                let ch = channel_at(pattern, x, y);
                assert_eq!(kind.is_green(x), ch == styx_format::Channel::Green);
                if ch != styx_format::Channel::Green {
                    assert_eq!(kind.x_is_red, ch == styx_format::Channel::Red);
                }
            }
        }
    }
}

#[test]
fn zone_sums_match() {
    for width in widths() {
        let rows: Vec<Vec<u16>> = (0..3)
            .map(|i| words(width, width as u64 * 3 + i, 4095))
            .collect();
        let rgb = [&rows[0][..], &rows[1][..], &rows[2][..]];
        for sat in [0u16, 1, 2000, 3890, 4095, 4096] {
            let want = scalar::zone_sums(rgb, width, sat);
            assert_eq!(
                zone_sums(rgb, width, sat).0,
                want,
                "width {width} sat {sat}"
            );
            for runner in runners() {
                #[allow(unused_variables)]
                let leaf: Option<(SimdBackend, usize, [u32; 5])> = match runner {
                    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
                    Runner::X86(f) => x86::zone_sums(f, rgb, width, sat),
                    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
                    Runner::Neon => neon::zone_sums(rgb, width, sat),
                    #[cfg(all(
                        feature = "neon",
                        feature = "std",
                        target_arch = "arm",
                        target_os = "linux"
                    ))]
                    Runner::Neon32 => neon32::zone_sums(rgb, width, sat),
                    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
                    Runner::Rvv => rvv::zone_sums(rgb, width, sat),
                    _ => None,
                };
                if let Some((_, done, sums)) = leaf {
                    assert_eq!(
                        sums,
                        scalar::zone_sums(rgb, done, sat),
                        "{runner:?} width {width}"
                    );
                }
            }
        }
    }
}

#[test]
fn luma_bins_match() {
    for bins in [1u16, 7, 64, 256, 1000, 4096] {
        check("luma bins", 1, 1, GUARD, |runner, w, out| {
            let rows: Vec<Vec<u16>> = (0..3)
                .map(|i| words(w, w as u64 * 3 + i + bins as u64, 4095))
                .collect();
            let rgb = [&rows[0][..], &rows[1][..], &rows[2][..]];
            let dst = out[0].as_mut_slice();
            match runner {
                Runner::Oracle => scalar::luma_bins_row(rgb, dst, bins, w),
                Runner::Dispatch => drop(luma_bins_row(rgb, dst, bins, w)),
                _ => return leaf_call!(runner, luma_bins_row(rgb, &mut dst[..w], bins, w)),
            }
            w
        });
    }
}
