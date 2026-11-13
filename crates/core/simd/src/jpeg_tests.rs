//! The JPEG kernels (`simd::jpeg`): the dispatchers (vector leaves where the CPU has them)
//! against the per-pixel and per-block scalar oracle, and the integer DCT against the real one.

use super::jpeg::*;
use super::{ColorLayout, SimdBackend};
use std::vec::Vec;

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

const LAYOUTS: [ColorLayout; 4] = [
    ColorLayout::Rgb24,
    ColorLayout::Bgr24,
    ColorLayout::Rgba32,
    ColorLayout::Bgra32,
];

#[test]
fn colour_rows_match_the_per_pixel_oracle() {
    for layout in LAYOUTS {
        let bpp = layout.bytes_per_pixel();
        let [ro, go, bo] = layout.rgb_offsets();
        for width in (1..=70).chain([127, 128, 129, 640, 1281]) {
            let (top, bottom) = (
                bytes(width * bpp, width as u64),
                bytes(width * bpp, !width as u64),
            );
            let half = width.div_ceil(2);
            // One guard byte after each output row.
            let (mut yt, mut yb) = (vec![0xA7; width + 1], vec![0xA7; width + 1]);
            let (mut cb, mut cr) = (vec![0xA7; half + 1], vec![0xA7; half + 1]);
            rgb_ycc420_rows(
                &top, &bottom, &mut yt, &mut yb, &mut cb, &mut cr, width, layout,
            );
            for x in 0..width {
                let px = |row: &[u8]| (row[x * bpp + ro], row[x * bpp + go], row[x * bpp + bo]);
                let ((r, g, b), (r2, g2, b2)) = (px(&top), px(&bottom));
                assert_eq!(yt[x], luma(r, g, b), "{layout:?} width {width} x {x}");
                assert_eq!(yb[x], luma(r2, g2, b2), "{layout:?} width {width} x {x}");
            }
            for i in 0..half {
                let (mut rs, mut gs, mut bs) = (0, 0, 0);
                for x in [2 * i, (2 * i + 1).min(width - 1)] {
                    for row in [&top, &bottom] {
                        rs += row[x * bpp + ro] as u32;
                        gs += row[x * bpp + go] as u32;
                        bs += row[x * bpp + bo] as u32;
                    }
                }
                assert_eq!(
                    (cb[i], cr[i]),
                    chroma(rs, gs, bs),
                    "{layout:?} width {width} pair {i}"
                );
            }
            assert_eq!(
                [yt[width], yb[width], cb[half], cr[half]],
                [0xA7; 4],
                "guard"
            );
        }
    }
}

#[test]
fn chroma_stays_in_range_without_a_clamp() {
    for (rs, gs, bs) in [
        (0u32, 0u32, 1020u32),
        (1020, 0, 0),
        (1020, 1020, 0),
        (0, 1020, 1020),
        (0, 0, 0),
    ] {
        let (r, g, b) = (rs as i32, gs as i32, bs as i32);
        let bias = (128 << 18) + (1 << 17) - 1;
        let cb = (-11059 * r - 21709 * g + 32768 * b + bias) >> 18;
        let cr = (32768 * r - 27439 * g - 5329 * b + bias) >> 18;
        assert!(
            (0..=255).contains(&cb) && (0..=255).contains(&cr),
            "{rs} {gs} {bs}"
        );
    }
    assert_eq!(chroma(510, 510, 510), (128, 128), "grey has no chroma");
    assert_eq!(luma(255, 255, 255), 255);
}

#[test]
fn bands_match_the_per_block_oracle() {
    let mut quant = [0u16; 64];
    for (i, q) in quant.iter_mut().enumerate() {
        *q = 1 + (i as u16 * 7) % 50;
    }
    for blocks in (1..=40).chain([63, 64, 65, 160]) {
        let stride = 8 * blocks + 5;
        let band = bytes(8 * stride, blocks as u64 + 11);
        let mut out = vec![0x5A5Au16 as i16; 64 * blocks + 1];
        let backend = fdct_quant_band(&band, stride, blocks, &quant, &mut out);
        for b in 0..blocks {
            let mut want = [0i16; 64];
            fdct_quant_block(&band[8 * b..], stride, &quant, &mut want);
            assert_eq!(
                &out[64 * b..64 * b + 64],
                &want,
                "{backend:?}: {blocks} blocks, block {b}"
            );
        }
        assert_eq!(out[64 * blocks], 0x5A5A, "guard");
    }
}

#[test]
fn extreme_blocks_fit_the_arithmetic() {
    let quant = [1u16; 64];
    for pattern in [[0u8, 255], [255, 0], [0, 0], [255, 255]] {
        // A checkerboard of the two values: the largest high frequencies.
        let block: Vec<u8> = (0..64).map(|i| pattern[(i / 8 + i % 8) % 2]).collect();
        let mut out = [0i16; 64];
        fdct_quant_band(&block, 8, 1, &quant, &mut out);
        let real = float_dct(&block);
        for k in 0..64 {
            assert!(
                (out[k] as f64 - real[k]).abs() <= 1.0,
                "{pattern:?} {k}: {} vs {}",
                out[k],
                real[k]
            );
        }
    }
}

#[test]
fn the_integer_dct_is_within_one_of_the_real_one() {
    let quant = [1u16; 64];
    let mut worst = 0f64;
    for seed in 0..200 {
        let block = bytes(64, seed);
        let mut out = [0i16; 64];
        fdct_quant_block(&block, 8, &quant, &mut out);
        let real = float_dct(&block);
        for k in 0..64 {
            worst = worst.max((out[k] as f64 - real[k]).abs());
        }
    }
    assert!(worst <= 1.0, "worst error {worst}");
    // A flat block has only its DC term: 8 · (s - 128).
    let mut out = [0i16; 64];
    fdct_quant_block(&[200; 64], 8, &quant, &mut out);
    assert_eq!(out[0], 576);
    assert!(out[1..].iter().all(|&c| c == 0));
}

#[test]
fn quantisation_rounds_half_away_from_zero() {
    assert_eq!(quantize(15, 10), 2);
    assert_eq!(quantize(14, 10), 1);
    assert_eq!(quantize(-15, 10), -2);
    assert_eq!(quantize(-14, 10), -1);
    assert_eq!(quantize(0, 3), 0);
    assert_eq!(quantize(-2048, 1), -2048);
}

#[test]
fn scalar_backend_reported_without_vectors() {
    let backend = fdct_quant_band(&[128; 64], 8, 1, &[1; 64], &mut [0; 64]);
    assert!(matches!(backend, SimdBackend::Scalar | SimdBackend::Rvv));
}

/// The real 8x8 DCT-II of JPEG, `F(v, u)` at index `8v + u`.
fn float_dct(block: &[u8]) -> [f64; 64] {
    use std::f64::consts::PI;
    let c = |k: usize| {
        if k == 0 {
            std::f64::consts::FRAC_1_SQRT_2
        } else {
            1.0
        }
    };
    let mut out = [0f64; 64];
    for v in 0..8 {
        for u in 0..8 {
            let mut sum = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    sum += (block[y * 8 + x] as f64 - 128.0)
                        * ((2 * x + 1) as f64 * u as f64 * PI / 16.0).cos()
                        * ((2 * y + 1) as f64 * v as f64 * PI / 16.0).cos();
                }
            }
            out[v * 8 + u] = c(u) * c(v) / 4.0 * sum;
        }
    }
    out
}
