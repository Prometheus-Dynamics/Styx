//! Every SIMD leaf against the scalar oracle: widths around each vector size, random data,
//! guard bytes after the row (nothing may be written past it), and the public dispatchers.

use super::*;

const GUARD: u8 = 0xA7;

const LAYOUTS: [ColorLayout; 4] = [
    ColorLayout::Rgb24,
    ColorLayout::Bgr24,
    ColorLayout::Rgba32,
    ColorLayout::Bgra32,
];

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

/// Widths around every block size the leaves use.
fn widths() -> impl Iterator<Item = usize> {
    (0..=100).chain([127, 128, 129, 255, 256, 257, 1280])
}

/// Run `leaf` (returning pixels done) on rows of `width` pixels and check its output prefix
/// against `oracle` and that the guard after the row is intact.
fn check_row(
    name: &str,
    src_bpp: usize,
    dst_bpp: usize,
    leaf: impl Fn(&[u8], &mut [u8], usize) -> usize,
    oracle: impl Fn(&[u8], &mut [u8], usize),
) {
    for width in widths() {
        let src = bytes(width * src_bpp, width as u64);
        let mut want = vec![0u8; width * dst_bpp];
        oracle(&src, &mut want, width);
        let mut got = vec![GUARD; width * dst_bpp + 64];
        let done = leaf(&src, &mut got[..width * dst_bpp], width);
        assert!(done <= width, "{name}: width {width}: did {done}");
        assert_eq!(
            got[..done * dst_bpp],
            want[..done * dst_bpp],
            "{name}: width {width}"
        );
        assert!(
            got[width * dst_bpp..].iter().all(|&b| b == GUARD),
            "{name}: width {width}: wrote past the row"
        );
    }
}

/// The public dispatchers against the oracles, for whatever backend this machine runs.
#[test]
fn dispatchers_match_the_oracle() {
    type Pair<'a> = (
        &'a str,
        usize,
        usize,
        Box<dyn Fn(&[u8], &mut [u8], usize)>,
        Box<dyn Fn(&[u8], &mut [u8], usize)>,
    );
    let cases: Vec<Pair> = vec![
        (
            "swap_rb24",
            3,
            3,
            Box::new(|s, d, w| {
                swap_rb24_row(s, d, w);
            }),
            Box::new(scalar::swap_rb24_row),
        ),
        (
            "swap_rb32",
            4,
            4,
            Box::new(|s, d, w| {
                swap_rb32_row(s, d, w);
            }),
            Box::new(scalar::swap_rb32_row),
        ),
        (
            "rgbx",
            4,
            3,
            Box::new(|s, d, w| {
                x32_to_rgb24_row(s, d, w, false);
            }),
            Box::new(|s, d, w| scalar::x32_to_rgb24_row(s, d, w, false)),
        ),
        (
            "bgrx",
            4,
            3,
            Box::new(|s, d, w| {
                x32_to_rgb24_row(s, d, w, true);
            }),
            Box::new(|s, d, w| scalar::x32_to_rgb24_row(s, d, w, true)),
        ),
        (
            "gray8",
            1,
            3,
            Box::new(|s, d, w| {
                gray8_to_rgb24_row(s, d, w);
            }),
            Box::new(scalar::gray8_to_rgb24_row),
        ),
        (
            "gray16",
            2,
            3,
            Box::new(|s, d, w| {
                gray16le_to_rgb24_row(s, d, w);
            }),
            Box::new(scalar::gray16le_to_rgb24_row),
        ),
        (
            "rgb48",
            6,
            3,
            Box::new(|s, d, w| {
                rgb48le_to_rgb24_row(s, d, w, false);
            }),
            Box::new(|s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, false)),
        ),
        (
            "bgr48",
            6,
            3,
            Box::new(|s, d, w| {
                rgb48le_to_rgb24_row(s, d, w, true);
            }),
            Box::new(|s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, true)),
        ),
        (
            "yuyv",
            2,
            1,
            Box::new(|s, d, w| {
                yuyv_luma_row(s, d, w);
            }),
            Box::new(scalar::yuyv_luma_row),
        ),
        (
            "reverse1",
            1,
            1,
            Box::new(|s, d, w| {
                reverse_row(s, d, w, 1);
            }),
            Box::new(|s, d, w| scalar::reverse_row(s, d, w, 1)),
        ),
        (
            "reverse2",
            2,
            2,
            Box::new(|s, d, w| {
                reverse_row(s, d, w, 2);
            }),
            Box::new(|s, d, w| scalar::reverse_row(s, d, w, 2)),
        ),
        (
            "reverse3",
            3,
            3,
            Box::new(|s, d, w| {
                reverse_row(s, d, w, 3);
            }),
            Box::new(|s, d, w| scalar::reverse_row(s, d, w, 3)),
        ),
        (
            "reverse4",
            4,
            4,
            Box::new(|s, d, w| {
                reverse_row(s, d, w, 4);
            }),
            Box::new(|s, d, w| scalar::reverse_row(s, d, w, 4)),
        ),
    ];
    for (name, sb, db, run, oracle) in cases {
        // The whole row must match: `done` = width.
        check_row(
            name,
            sb,
            db,
            |s, d, w| {
                run(s, d, w);
                w
            },
            oracle,
        );
    }
    for layout in LAYOUTS {
        let bpp = layout.bytes_per_pixel();
        check_row(
            "luma",
            bpp,
            1,
            |s, d, w| {
                rgb_to_luma_row(s, d, w, layout);
                w
            },
            |s, d, w| scalar::rgb_to_luma_row(s, d, w, layout),
        );
    }
    check_row(
        "rgba",
        3,
        4,
        |s, d, w| {
            rgb24_to_rgba_row(s, d, w);
            w
        },
        scalar::rgb24_to_rgba_row,
    );
    for width in widths() {
        let (top, bottom) = (bytes(width * 2, 1), bytes(width * 2, 2));
        let (mut got, mut want) = (vec![0; width], vec![0; width]);
        box2_row(&top, &bottom, &mut got, width);
        scalar::box2_row(&top, &bottom, &mut want, width);
        assert_eq!(got, want, "box2 width {width}");
        yuyv_chroma_row(&top, &bottom, &mut got, width);
        scalar::yuyv_chroma_row(&top, &bottom, &mut want, width);
        assert_eq!(got, want, "yuyv chroma width {width}");
    }
}

#[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
mod x86_leaves {
    use super::super::x86::rows;
    use super::*;

    #[test]
    fn row_leaves_match_the_oracle() {
        let f = X86FeatureSet::detect();
        // SAFETY: each leaf runs only when its feature was detected.
        if f.sse2 {
            check_row(
                "swap_rb32 sse2",
                4,
                4,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::swap_rb32_sse2(s, d, w) },
                scalar::swap_rb32_row,
            );
            check_row(
                "rgb48 sse2",
                6,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::rgb48le_to_rgb24_sse2(s, d, w) },
                |s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, false),
            );
            check_row(
                "yuyv sse2",
                2,
                1,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::yuyv_luma_sse2(s, d, w) },
                scalar::yuyv_luma_row,
            );
            for bpp in [1, 2, 4] {
                check_row(
                    "reverse sse2",
                    bpp,
                    bpp,
                    |s, d, w| {
                        // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                        let n = unsafe { rows::reverse_sse2(s, d, w, bpp) };
                        finish_reverse(s, d, w, bpp, 0..n)
                    },
                    |s, d, w| scalar::reverse_row(s, d, w, bpp),
                );
            }
            // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
            box2_leaf("box2 sse2", |t, b, d, w| unsafe {
                rows::box2_sse2(t, b, d, w)
            });
            // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
            chroma_leaf("yuyv chroma sse2", |t, b, d, w| unsafe {
                rows::yuyv_chroma_sse2(t, b, d, w)
            });
        }
        if f.ssse3 {
            for layout in LAYOUTS {
                check_row(
                    "luma ssse3",
                    layout.bytes_per_pixel(),
                    1,
                    // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                    |s, d, w| unsafe { rows::rgb_to_luma_ssse3(s, d, w, layout) },
                    |s, d, w| scalar::rgb_to_luma_row(s, d, w, layout),
                );
            }
            check_row(
                "rgba ssse3",
                3,
                4,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::rgb24_to_rgba_ssse3(s, d, w) },
                scalar::rgb24_to_rgba_row,
            );
            check_row(
                "swap_rb32 ssse3",
                4,
                4,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::swap_rb32_ssse3(s, d, w) },
                scalar::swap_rb32_row,
            );
            check_row(
                "swap_rb24 ssse3",
                3,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::swap_rb24_ssse3(s, d, w) },
                scalar::swap_rb24_row,
            );
            for swap in [false, true] {
                check_row(
                    "x32 ssse3",
                    4,
                    3,
                    // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                    |s, d, w| unsafe { rows::x32_to_rgb24_ssse3(s, d, w, swap) },
                    |s, d, w| scalar::x32_to_rgb24_row(s, d, w, swap),
                );
            }
            check_row(
                "gray8 ssse3",
                1,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::gray8_to_rgb24_ssse3(s, d, w) },
                scalar::gray8_to_rgb24_row,
            );
            check_row(
                "gray16 ssse3",
                2,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::gray16le_to_rgb24_ssse3(s, d, w) },
                scalar::gray16le_to_rgb24_row,
            );
            check_row(
                "bgr48 ssse3",
                6,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::rgb48le_to_rgb24_swap_ssse3(s, d, w) },
                |s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, true),
            );
            check_row(
                "reverse1 ssse3",
                1,
                1,
                |s, d, w| {
                    // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                    let n = unsafe { rows::reverse_u8_ssse3(s, d, w) };
                    finish_reverse(s, d, w, 1, 0..n)
                },
                |s, d, w| scalar::reverse_row(s, d, w, 1),
            );
            check_row(
                "reverse3 ssse3",
                3,
                3,
                |s, d, w| {
                    // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                    let (a, b) = unsafe { rows::reverse_rgb_ssse3(s, d, w) };
                    finish_reverse(s, d, w, 3, a..b)
                },
                |s, d, w| scalar::reverse_row(s, d, w, 3),
            );
        }
        if f.avx2 {
            check_row(
                "swap_rb32 avx2",
                4,
                4,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::swap_rb32_avx2(s, d, w) },
                scalar::swap_rb32_row,
            );
            check_row(
                "swap_rb24 avx2",
                3,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::swap_rb24_avx2(s, d, w) },
                scalar::swap_rb24_row,
            );
            for swap in [false, true] {
                check_row(
                    "x32 avx2",
                    4,
                    3,
                    // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                    |s, d, w| unsafe { rows::x32_to_rgb24_avx2(s, d, w, swap) },
                    |s, d, w| scalar::x32_to_rgb24_row(s, d, w, swap),
                );
            }
            check_row(
                "gray8 avx2",
                1,
                3,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::gray8_to_rgb24_avx2(s, d, w) },
                scalar::gray8_to_rgb24_row,
            );
            check_row(
                "yuyv avx2",
                2,
                1,
                // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::yuyv_luma_avx2(s, d, w) },
                scalar::yuyv_luma_row,
            );
            // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
            box2_leaf("box2 avx2", |t, b, d, w| unsafe {
                rows::box2_avx2(t, b, d, w)
            });
            // SAFETY: the enclosing test detected this leaf's feature before running it; the buffers are sized for the widths it passes.
            chroma_leaf("yuyv chroma avx2", |t, b, d, w| unsafe {
                rows::yuyv_chroma_avx2(t, b, d, w)
            });
        }
    }
}

#[cfg(all(feature = "neon", target_arch = "aarch64"))]
mod neon_leaves {
    use super::super::neon::rows;
    use super::*;

    #[test]
    fn row_leaves_match_the_oracle() {
        // SAFETY: NEON is part of the AArch64 baseline.
        for layout in LAYOUTS {
            check_row(
                "luma neon",
                layout.bytes_per_pixel(),
                1,
                // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::rgb_to_luma(s, d, w, layout) },
                |s, d, w| scalar::rgb_to_luma_row(s, d, w, layout),
            );
        }
        check_row(
            "rgba neon",
            3,
            4,
            // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
            |s, d, w| unsafe { rows::rgb24_to_rgba(s, d, w) },
            scalar::rgb24_to_rgba_row,
        );
        check_row(
            "swap_rb32 neon",
            4,
            4,
            // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
            |s, d, w| unsafe { rows::swap_rb32(s, d, w) },
            scalar::swap_rb32_row,
        );
        check_row(
            "swap_rb24 neon",
            3,
            3,
            // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
            |s, d, w| unsafe { rows::swap_rb24(s, d, w) },
            scalar::swap_rb24_row,
        );
        for swap in [false, true] {
            check_row(
                "x32 neon",
                4,
                3,
                // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::x32_to_rgb24(s, d, w, swap) },
                |s, d, w| scalar::x32_to_rgb24_row(s, d, w, swap),
            );
            check_row(
                "rgb48 neon",
                6,
                3,
                // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
                |s, d, w| unsafe { rows::rgb48le_to_rgb24(s, d, w, swap) },
                |s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, swap),
            );
        }
        check_row(
            "gray8 neon",
            1,
            3,
            // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
            |s, d, w| unsafe { rows::gray8_to_rgb24(s, d, w) },
            scalar::gray8_to_rgb24_row,
        );
        check_row(
            "gray16 neon",
            2,
            3,
            // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
            |s, d, w| unsafe { rows::gray16le_to_rgb24(s, d, w) },
            scalar::gray16le_to_rgb24_row,
        );
        check_row(
            "yuyv neon",
            2,
            1,
            // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
            |s, d, w| unsafe { rows::yuyv_luma(s, d, w) },
            scalar::yuyv_luma_row,
        );
        for bpp in 1..=4 {
            check_row(
                "reverse neon",
                bpp,
                bpp,
                |s, d, w| {
                    // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
                    let n = unsafe { rows::reverse(s, d, w, bpp) };
                    finish_reverse(s, d, w, bpp, 0..n)
                },
                |s, d, w| scalar::reverse_row(s, d, w, bpp),
            );
        }
        // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
        box2_leaf("box2 neon", |t, b, d, w| unsafe { rows::box2(t, b, d, w) });
        // SAFETY: the guard above detected this leaf's feature before the test runs it; the buffers are sized for the widths it passes.
        chroma_leaf("yuyv chroma neon", |t, b, d, w| unsafe {
            rows::yuyv_chroma(t, b, d, w)
        });
    }
}

/// Complete a reverse leaf's partial result (it did source pixels `done`) so `check_row` can
/// compare the whole row; returns `width`.
#[allow(dead_code)]
fn finish_reverse(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
    done: std::ops::Range<usize>,
) -> usize {
    for i in (0..done.start).chain(done.end..width) {
        let d = (width - 1 - i) * bpp;
        dst[d..d + bpp].copy_from_slice(&src[i * bpp..(i + 1) * bpp]);
    }
    width
}

#[allow(dead_code)]
fn box2_leaf(name: &str, leaf: impl Fn(&[u8], &[u8], &mut [u8], usize) -> usize) {
    pair_leaf(name, scalar::box2_row, leaf);
}

/// YUYV chroma leaf against the scalar kernel.
#[allow(dead_code)]
fn chroma_leaf(name: &str, leaf: impl Fn(&[u8], &[u8], &mut [u8], usize) -> usize) {
    pair_leaf(name, scalar::yuyv_chroma_row, leaf);
}

/// A leaf of two `2 * width`-byte rows into `width` bytes against `oracle`, writing nothing
/// past `width`.
#[allow(dead_code)]
fn pair_leaf(
    name: &str,
    oracle: fn(&[u8], &[u8], &mut [u8], usize),
    leaf: impl Fn(&[u8], &[u8], &mut [u8], usize) -> usize,
) {
    for width in widths() {
        let (top, bottom) = (bytes(width * 2, 3), bytes(width * 2, 4));
        let mut want = vec![0; width];
        oracle(&top, &bottom, &mut want, width);
        let mut got = vec![GUARD; width + 64];
        let done = leaf(&top, &bottom, &mut got[..width], width);
        assert_eq!(got[..done], want[..done], "{name} width {width}");
        assert!(
            got[width..].iter().all(|&b| b == GUARD),
            "{name} width {width}"
        );
    }
}

/// Every row leaf of a run-time-detected backend module (`neon32`, `rvv`) against
/// the oracle; `$backend` is the one it reports.
#[allow(unused_macros)]
macro_rules! detected_leaves {
    ($m:ident, $backend:expr) => {
        let done = |r: Option<(SimdBackend, usize)>| {
            r.map_or(0, |(b, n)| {
                assert_eq!(b, $backend);
                n
            })
        };
        check_row(
            "swap_rb24",
            3,
            3,
            |s, d, w| done($m::swap_rb24_row(s, d, w)),
            scalar::swap_rb24_row,
        );
        check_row(
            "swap_rb32",
            4,
            4,
            |s, d, w| done($m::swap_rb32_row(s, d, w)),
            scalar::swap_rb32_row,
        );
        for swap in [false, true] {
            check_row(
                "x32",
                4,
                3,
                |s, d, w| done($m::x32_to_rgb24_row(s, d, w, swap)),
                |s, d, w| scalar::x32_to_rgb24_row(s, d, w, swap),
            );
            check_row(
                "rgb48",
                6,
                3,
                |s, d, w| done($m::rgb48le_to_rgb24_row(s, d, w, swap)),
                |s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, swap),
            );
        }
        for layout in LAYOUTS {
            check_row(
                "luma",
                layout.bytes_per_pixel(),
                1,
                |s, d, w| done($m::rgb_to_luma_row(s, d, w, layout)),
                |s, d, w| scalar::rgb_to_luma_row(s, d, w, layout),
            );
        }
        check_row(
            "rgba",
            3,
            4,
            |s, d, w| done($m::rgb24_to_rgba_row(s, d, w)),
            scalar::rgb24_to_rgba_row,
        );
        check_row(
            "gray8",
            1,
            3,
            |s, d, w| done($m::gray8_to_rgb24_row(s, d, w)),
            scalar::gray8_to_rgb24_row,
        );
        check_row(
            "gray16",
            2,
            3,
            |s, d, w| done($m::gray16le_to_rgb24_row(s, d, w)),
            scalar::gray16le_to_rgb24_row,
        );
        check_row(
            "yuyv",
            2,
            1,
            |s, d, w| done($m::yuyv_luma_row(s, d, w)),
            scalar::yuyv_luma_row,
        );
        for bpp in 1..=4 {
            check_row(
                "reverse",
                bpp,
                bpp,
                |s, d, w| match $m::reverse_row(s, d, w, bpp) {
                    Some((_, range)) => finish_reverse(s, d, w, bpp, range),
                    None => finish_reverse(s, d, w, bpp, 0..0),
                },
                |s, d, w| scalar::reverse_row(s, d, w, bpp),
            );
        }
        box2_leaf("box2", |t, b, d, w| done($m::box2_row(t, b, d, w)));
        chroma_leaf("yuyv chroma", |t, b, d, w| {
            done($m::yuyv_chroma_row(t, b, d, w))
        });
        // Saturated input: the widest sum (4 * 255 + 2) narrows back to 255.
        let row = vec![255u8; 2 * 300];
        let mut all = vec![0u8; 300];
        assert_eq!(box2_row(&row, &row, &mut all, 300), $backend);
        assert!(all.iter().all(|&b| b == 255));
        assert_eq!(strongest_backend(), $backend);
    };
}

#[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
#[test]
fn rvv_leaves_match_scalar() {
    assert!(has_rvv(), "the riscv gate runs QEMU with v=true");
    detected_leaves!(rvv, SimdBackend::Rvv);
}

#[cfg(all(
    feature = "neon",
    feature = "std",
    target_arch = "arm",
    target_os = "linux"
))]
#[test]
fn neon32_leaves_match_scalar() {
    assert!(has_neon32(), "the armv7 gate runs QEMU on a Cortex-A7");
    detected_leaves!(neon32, SimdBackend::Neon);
}
