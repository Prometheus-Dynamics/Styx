//! `GpuConvert` against `styx_core::simd`'s scalar kernels, byte for byte: every conversion, odd
//! and even sizes from one pixel up, strides with and without padding, random data (and the
//! YUV range edges), on every Vulkan device of the machine. The destination starts as a
//! sentinel, so the bytes between rows must come back untouched too. Without Vulkan the tests
//! pass with a note.

use crate::common::contexts;
use styx_core::simd::{ColorLayout, Orientation, YuvMatrix, scalar, transform_packed};
use styx_gpuisp::{GpuContext, GpuConvert, Image, ImageMut};

const SENTINEL: u8 = 0xEE;

/// Odd and even widths and heights, one pixel to a few tens.
const SIZES: [(usize, usize); 11] = [
    (1, 1),
    (2, 1),
    (1, 2),
    (3, 3),
    (2, 2),
    (5, 4),
    (8, 8),
    (9, 7),
    (17, 5),
    (33, 4),
    (64, 31),
];

/// Padding after each row, in bytes: none, and an odd amount.
const PADS: [usize; 2] = [0, 3];

/// xorshift32 bytes.
fn random(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed.wrapping_mul(2_654_435_761) | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 24) as u8
        })
        .collect()
}

/// Random bytes with a quarter of them the YUV range's edges (clamps and offsets).
fn yuv_bytes(n: usize, seed: u32) -> Vec<u8> {
    const EDGES: [u8; 12] = [0, 1, 15, 16, 17, 127, 128, 129, 234, 235, 236, 255];
    random(n, seed)
        .into_iter()
        .map(|b| {
            if b % 4 == 0 {
                EDGES[usize::from(b / 4) % EDGES.len()]
            } else {
                b
            }
        })
        .collect()
}

/// `rows` source rows of `row` bytes, `pad` bytes of padding after each: the bytes and the
/// stride.
fn source(row: usize, rows: usize, pad: usize, seed: u32) -> (Vec<u8>, usize) {
    let stride = row + pad;
    (random(stride * rows, seed), stride)
}

/// A destination of `rows` rows at `stride`, all sentinel.
fn sentinel(stride: usize, rows: usize) -> Vec<u8> {
    vec![SENTINEL; stride * rows]
}

/// Calls `f(size, pad, seed)` for every size and padding.
fn each_case(mut f: impl FnMut((usize, usize), usize, u32)) {
    for (i, &size) in SIZES.iter().enumerate() {
        for (j, &pad) in PADS.iter().enumerate() {
            f(size, pad, (i * 7 + j * 3 + 1) as u32);
        }
    }
}

/// Byte-for-byte equality, naming the first difference.
fn same(ctx: &GpuContext, what: &str, size: (usize, usize), pad: usize, got: &[u8], want: &[u8]) {
    assert_eq!(got.len(), want.len(), "{what}: lengths");
    if let Some(i) = got.iter().zip(want).position(|(g, w)| g != w) {
        panic!(
            "{what} {}x{} pad {pad} on {}: byte {i} is {:#04x}, scalar {:#04x}",
            size.0,
            size.1,
            ctx.info().name,
            got[i],
            want[i]
        );
    }
}

/// Packed 1-to-1 conversions: `row_in` bytes in, `row_out` bytes out a row, `kernel` the
/// scalar row kernel, `gpu` the GPU one.
fn packed(
    name: &str,
    row_in: impl Fn(usize) -> usize,
    row_out: impl Fn(usize) -> usize,
    kernel: impl Fn(&[u8], &mut [u8], usize),
    gpu: impl Fn(
        &mut GpuConvert,
        Image<'_>,
        ImageMut<'_>,
        (usize, usize),
    ) -> Result<(), styx_gpuisp::GpuError>,
) {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        each_case(|(w, h), pad, seed| {
            let (src, ss) = source(row_in(w), h, pad, seed);
            let ds = row_out(w) + pad;
            let mut want = sentinel(ds, h);
            for y in 0..h {
                kernel(&src[y * ss..], &mut want[y * ds..], w);
            }
            let mut got = sentinel(ds, h);
            gpu(
                &mut c,
                Image {
                    data: &src,
                    stride: ss,
                },
                ImageMut {
                    data: &mut got,
                    stride: ds,
                },
                (w, h),
            )
            .unwrap();
            same(&ctx, name, (w, h), pad, &got, &want);
        });
    }
}

#[test]
fn swap_rb24_matches_scalar() {
    packed(
        "swap_rb24",
        |w| 3 * w,
        |w| 3 * w,
        scalar::swap_rb24_row,
        |c, s, d, size| c.swap_rb24(s, d, size),
    );
}

#[test]
fn swap_rb32_matches_scalar() {
    packed(
        "swap_rb32",
        |w| 4 * w,
        |w| 4 * w,
        scalar::swap_rb32_row,
        |c, s, d, size| c.swap_rb32(s, d, size),
    );
}

#[test]
fn x32_to_rgb24_matches_scalar() {
    for swap in [false, true] {
        packed(
            if swap {
                "bgrx_to_rgb24"
            } else {
                "xrgb_to_rgb24"
            },
            |w| 4 * w,
            |w| 3 * w,
            |s, d, w| scalar::x32_to_rgb24_row(s, d, w, swap),
            |c, s, d, size| c.x32_to_rgb24(s, d, size, swap),
        );
    }
}

#[test]
fn rgb_to_luma_matches_scalar() {
    for layout in [
        ColorLayout::Rgb24,
        ColorLayout::Bgr24,
        ColorLayout::Rgba32,
        ColorLayout::Bgra32,
    ] {
        let bpp = layout.bytes_per_pixel();
        packed(
            &format!("{layout:?} luma"),
            |w| bpp * w,
            |w| w,
            |s, d, w| scalar::rgb_to_luma_row(s, d, w, layout),
            |c, s, d, size| c.rgb_to_luma(s, d, size, layout),
        );
    }
}

#[test]
fn rgb24_to_rgba_matches_scalar() {
    packed(
        "rgb24_to_rgba",
        |w| 3 * w,
        |w| 4 * w,
        scalar::rgb24_to_rgba_row,
        |c, s, d, size| c.rgb24_to_rgba(s, d, size),
    );
}

#[test]
fn gray8_to_rgb24_matches_scalar() {
    packed(
        "gray8_to_rgb24",
        |w| w,
        |w| 3 * w,
        scalar::gray8_to_rgb24_row,
        |c, s, d, size| c.gray8_to_rgb24(s, d, size),
    );
}

#[test]
fn gray16le_to_rgb24_matches_scalar() {
    packed(
        "gray16le_to_rgb24",
        |w| 2 * w,
        |w| 3 * w,
        scalar::gray16le_to_rgb24_row,
        |c, s, d, size| c.gray16le_to_rgb24(s, d, size),
    );
}

#[test]
fn rgb48le_to_rgb24_matches_scalar() {
    for swap in [false, true] {
        packed(
            if swap {
                "bgr48_to_rgb24"
            } else {
                "rgb48_to_rgb24"
            },
            |w| 6 * w,
            |w| 3 * w,
            |s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, swap),
            |c, s, d, size| c.rgb48le_to_rgb24(s, d, size, swap),
        );
    }
}

#[test]
fn yuyv_to_luma_matches_scalar() {
    packed(
        "yuyv_to_luma",
        |w| 2 * w,
        |w| w,
        scalar::yuyv_luma_row,
        |c, s, d, size| c.yuyv_to_luma(s, d, size),
    );
}

#[test]
fn reverse_rows_matches_scalar() {
    for bpp in 1..=4 {
        packed(
            &format!("reverse {bpp}-byte pixels"),
            |w| bpp * w,
            |w| bpp * w,
            |s, d, w| scalar::reverse_row(s, d, w, bpp),
            |c, s, d, size| c.reverse_rows(s, d, size, bpp),
        );
    }
}

#[test]
fn yuyv_to_nv12_chroma_matches_scalar() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        each_case(|(w, h), pad, seed| {
            let (src, ss) = source(2 * w, h, pad, seed);
            let (rows, ds) = (h.div_ceil(2), w + pad);
            let mut want = sentinel(ds, rows);
            for r in 0..rows {
                // The last row of an odd height pairs with itself.
                let b = (2 * r + 1).min(h - 1);
                scalar::yuyv_chroma_row(&src[2 * r * ss..], &src[b * ss..], &mut want[r * ds..], w);
            }
            let mut got = sentinel(ds, rows);
            c.yuyv_to_nv12_chroma(
                Image {
                    data: &src,
                    stride: ss,
                },
                ImageMut {
                    data: &mut got,
                    stride: ds,
                },
                (w, h),
            )
            .unwrap();
            same(&ctx, "yuyv_to_nv12_chroma", (w, h), pad, &got, &want);
        });
    }
}

#[test]
fn transform_matches_scalar() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        for bpp in 1..=4 {
            for transpose in [false, true] {
                for flip_x in [false, true] {
                    for flip_y in [false, true] {
                        let o = Orientation {
                            transpose,
                            flip_x,
                            flip_y,
                        };
                        each_case(|(w, h), pad, seed| {
                            let (src, ss) = source(w * bpp, h, pad, seed);
                            let (wo, ho) = if transpose { (h, w) } else { (w, h) };
                            let ds = wo * bpp + pad;
                            let mut want = sentinel(ds, ho);
                            oracle(&src, ss, &mut want, ds, (w, h), bpp, o);
                            // The CPU path the pipelines run agrees with the oracle.
                            let mut cpu = sentinel(ds, ho);
                            transform_packed(&src, ss, &mut cpu, ds, (w, h), bpp, o);
                            assert!(cpu == want, "oracle and transform_packed differ: {o:?}");
                            let mut got = sentinel(ds, ho);
                            c.transform(
                                Image {
                                    data: &src,
                                    stride: ss,
                                },
                                ImageMut {
                                    data: &mut got,
                                    stride: ds,
                                },
                                (w, h),
                                bpp,
                                o,
                            )
                            .unwrap();
                            same(
                                &ctx,
                                &format!("transform {o:?} bpp {bpp}"),
                                (w, h),
                                pad,
                                &got,
                                &want,
                            );
                        });
                    }
                }
            }
        }
    }
}

/// Reorientation per `Orientation`'s definition: the output pixel (x, y) reads the source
/// pixel found by transposing (the source's rows are the output's columns) and then mirroring.
fn oracle(
    src: &[u8],
    ss: usize,
    dst: &mut [u8],
    ds: usize,
    (w, h): (usize, usize),
    bpp: usize,
    o: Orientation,
) {
    let (wo, ho) = if o.transpose { (h, w) } else { (w, h) };
    for y in 0..ho {
        for x in 0..wo {
            let (ix, iy) = if o.transpose {
                (
                    if o.flip_y { ho - 1 - y } else { y },
                    if o.flip_x { wo - 1 - x } else { x },
                )
            } else {
                (
                    if o.flip_x { w - 1 - x } else { x },
                    if o.flip_y { h - 1 - y } else { y },
                )
            };
            dst[y * ds + x * bpp..][..bpp].copy_from_slice(&src[iy * ss + ix * bpp..][..bpp]);
        }
    }
}

/// The three YUV matrices the decoders use, and their limited and full ranges.
const MATRICES: [YuvMatrix; 3] = [YuvMatrix::BT709, YuvMatrix::BT2020, YuvMatrix::BT601_FULL];

#[test]
fn nv12_to_rgb_matches_scalar() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        for m in &MATRICES {
            each_case(|(w, h), pad, seed| {
                let ys = w + pad;
                let y = yuv_bytes(ys * h, seed);
                let (uv_row, uv_rows) = (2 * w.div_ceil(2), h.div_ceil(2));
                let uv: Vec<u8> = yuv_bytes(uv_rows * (uv_row + pad), seed + 1);
                let us = uv_row + pad;
                let ds = 3 * w + pad;
                let mut want = sentinel(ds, h);
                for r in 0..h {
                    scalar::nv12_rgb_row(
                        &y[r * ys..],
                        &uv[(r / 2) * us..],
                        &mut want[r * ds..],
                        w,
                        m,
                    );
                }
                let mut got = sentinel(ds, h);
                c.nv12_to_rgb(
                    Image {
                        data: &y,
                        stride: ys,
                    },
                    Image {
                        data: &uv,
                        stride: us,
                    },
                    ImageMut {
                        data: &mut got,
                        stride: ds,
                    },
                    (w, h),
                    m,
                )
                .unwrap();
                same(
                    &ctx,
                    &format!("nv12_to_rgb {m:?}"),
                    (w, h),
                    pad,
                    &got,
                    &want,
                );
            });
        }
    }
}

#[test]
fn yuyv_to_rgb_matches_scalar() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        for m in &MATRICES {
            each_case(|(w, h), pad, seed| {
                let ss = 4 * w.div_ceil(2) + pad;
                let src = yuv_bytes(ss * h, seed + 9);
                let ds = 3 * w + pad;
                let mut want = sentinel(ds, h);
                for r in 0..h {
                    scalar::yuyv_rgb_row(&src[r * ss..], &mut want[r * ds..], w, m);
                }
                let mut got = sentinel(ds, h);
                c.yuyv_to_rgb(
                    Image {
                        data: &src,
                        stride: ss,
                    },
                    ImageMut {
                        data: &mut got,
                        stride: ds,
                    },
                    (w, h),
                    m,
                )
                .unwrap();
                same(
                    &ctx,
                    &format!("yuyv_to_rgb {m:?}"),
                    (w, h),
                    pad,
                    &got,
                    &want,
                );
            });
        }
    }
}

#[test]
fn i420_to_rgb_matches_scalar() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        for m in &MATRICES {
            each_case(|(w, h), pad, seed| {
                let ys = w + pad;
                let y = yuv_bytes(ys * h, seed);
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let cs = cw + pad;
                let u = yuv_bytes(ch * cs, seed + 2);
                let v = yuv_bytes(ch * cs, seed + 3);
                let ds = 3 * w + pad;
                let mut want = sentinel(ds, h);
                for r in 0..h {
                    scalar::i420_rgb_row(
                        &y[r * ys..],
                        &u[(r / 2) * cs..],
                        &v[(r / 2) * cs..],
                        &mut want[r * ds..],
                        w,
                        m,
                    );
                }
                let mut got = sentinel(ds, h);
                c.i420_to_rgb(
                    Image {
                        data: &y,
                        stride: ys,
                    },
                    Image {
                        data: &u,
                        stride: cs,
                    },
                    Image {
                        data: &v,
                        stride: cs,
                    },
                    ImageMut {
                        data: &mut got,
                        stride: ds,
                    },
                    (w, h),
                    m,
                )
                .unwrap();
                same(
                    &ctx,
                    &format!("i420_to_rgb {m:?}"),
                    (w, h),
                    pad,
                    &got,
                    &want,
                );
            });
        }
    }
}
