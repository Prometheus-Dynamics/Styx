//! `GpuConvert::jpeg_blocks` against `styx_core::simd::jpeg`, coefficient for coefficient: the
//! planes padded as the CPU encoder pads them, then `fdct_quant_band`, over odd and even sizes,
//! padded strides, every packed layout, grey, I420 and NV12, random pixels and quantisers.

use crate::common::contexts;
use styx_core::simd::ColorLayout;
use styx_core::simd::jpeg::{fdct_quant_band, rgb_ycc420_rows};
use styx_gpuisp::{GpuConvert, Image, JpegGrid, JpegPixels};

const SIZES: [(usize, usize); 9] = [
    (1, 1),
    (2, 3),
    (7, 5),
    (16, 16),
    (17, 9),
    (33, 31),
    (40, 17),
    (64, 48),
    (129, 66),
];

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

fn quant(seed: u32) -> [[u16; 64]; 2] {
    let r = random(128, seed);
    let mut q = [[0u16; 64]; 2];
    for (i, v) in q.iter_mut().flatten().enumerate() {
        // Mostly small (large coefficients survive), some up to 255.
        *v = if i % 5 == 0 {
            1 + u16::from(r[i])
        } else {
            1 + u16::from(r[i] % 16)
        };
    }
    q
}

/// The padded planes: luma `pw` x `ph`, chroma half that.
struct Padded {
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
    pw: usize,
    ph: usize,
}

fn pad_row(row: &mut [u8], n: usize) {
    let last = row[n - 1];
    row[n..].fill(last);
}

/// `w` x `h` plane (rows `stride` apart) into `pw` x `ph`, edges repeated.
fn pad_plane(
    src: &[u8],
    stride: usize,
    (w, h): (usize, usize),
    (pw, ph): (usize, usize),
) -> Vec<u8> {
    let mut out = vec![0; pw * ph];
    for r in 0..ph {
        let row = &mut out[r * pw..][..pw];
        row[..w].copy_from_slice(&src[r.min(h - 1) * stride..][..w]);
        pad_row(row, w);
    }
    out
}

/// The CPU's blocks of `p` in the GPU's grid layout.
fn oracle(p: &Padded, gray: bool, q: &[[u16; 64]; 2]) -> Vec<i16> {
    let grid = JpegGrid::new((p.pw, p.ph), gray);
    let mut out = vec![0i16; grid.blocks() * 64];
    let bw = p.pw / 8;
    for band in 0..p.ph / 8 {
        fdct_quant_band(
            &p.y[band * 8 * p.pw..],
            p.pw,
            bw,
            &q[0],
            &mut out[band * bw * 64..],
        );
    }
    if !gray {
        let cw = p.pw / 2;
        for band in 0..p.ph / 16 {
            let at = (grid.luma_rows + band) * bw * 64;
            let (cb, cr) = out[at..][..bw * 64].split_at_mut(bw * 32);
            fdct_quant_band(&p.cb[band * 8 * cw..], cw, bw / 2, &q[1], cb);
            fdct_quant_band(&p.cr[band * 8 * cw..], cw, bw / 2, &q[1], cr);
        }
    }
    out
}

fn padded_size((w, h): (usize, usize), mcu: usize) -> (usize, usize) {
    (w.next_multiple_of(mcu), h.next_multiple_of(mcu))
}

#[test]
fn packed_blocks_match_the_cpu() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        let mut got = Vec::new();
        for layout in [
            ColorLayout::Rgb24,
            ColorLayout::Bgr24,
            ColorLayout::Rgba32,
            ColorLayout::Bgra32,
        ] {
            for (i, &(w, h)) in SIZES.iter().enumerate() {
                let bpp = layout.bytes_per_pixel();
                let stride = w * bpp + 3 * (i % 2);
                let data = random(stride * h, i as u32 + 7);
                let (pw, ph) = padded_size((w, h), 16);
                let (cw, real_cw) = (pw / 2, w.div_ceil(2));
                let mut p = Padded {
                    y: vec![0; pw * ph],
                    cb: vec![0; cw * ph / 2],
                    cr: vec![0; cw * ph / 2],
                    pw,
                    ph,
                };
                let line = |r: usize| &data[r.min(h - 1) * stride..][..w * bpp];
                for pair in 0..ph / 2 {
                    let (yt, yb) = p.y[2 * pair * pw..][..2 * pw].split_at_mut(pw);
                    let (cb, cr) = (&mut p.cb[pair * cw..][..cw], &mut p.cr[pair * cw..][..cw]);
                    rgb_ycc420_rows(
                        line(2 * pair),
                        line(2 * pair + 1),
                        yt,
                        yb,
                        cb,
                        cr,
                        w,
                        layout,
                    );
                    pad_row(yt, w);
                    pad_row(yb, w);
                    pad_row(cb, real_cw);
                    pad_row(cr, real_cw);
                }
                let q = quant(i as u32);
                let image = Image {
                    data: &data,
                    stride,
                };
                let grid = c
                    .jpeg_blocks(JpegPixels::Packed { image, layout }, (w, h), &q, &mut got)
                    .unwrap();
                assert_eq!(grid, JpegGrid::new((w, h), false));
                assert!(got == oracle(&p, false, &q), "{layout:?} {w}x{h}");
            }
        }
    }
}

#[test]
fn planar_blocks_match_the_cpu() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        let mut got = Vec::new();
        for (i, &(w, h)) in SIZES.iter().enumerate() {
            let pad = 5 * (i % 2);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let q = quant(i as u32 + 100);
            let ys = w + pad;
            let y = random(ys * h, i as u32);

            // Grey.
            let (pw, ph) = padded_size((w, h), 8);
            let p = Padded {
                y: pad_plane(&y, ys, (w, h), (pw, ph)),
                cb: Vec::new(),
                cr: Vec::new(),
                pw,
                ph,
            };
            let image = Image {
                data: &y,
                stride: ys,
            };
            c.jpeg_blocks(JpegPixels::Gray(image), (w, h), &q, &mut got)
                .unwrap();
            assert!(got == oracle(&p, true, &q), "grey {w}x{h}");

            // I420 and NV12 of the same planes.
            let cs = cw + pad;
            let (u, v) = (random(cs * ch, i as u32 + 1), random(cs * ch, i as u32 + 2));
            let (pw, ph) = padded_size((w, h), 16);
            let p = Padded {
                y: pad_plane(&y, ys, (w, h), (pw, ph)),
                cb: pad_plane(&u, cs, (cw, ch), (pw / 2, ph / 2)),
                cr: pad_plane(&v, cs, (cw, ch), (pw / 2, ph / 2)),
                pw,
                ph,
            };
            let want = oracle(&p, false, &q);
            let pixels = JpegPixels::I420 {
                y: image,
                u: Image {
                    data: &u,
                    stride: cs,
                },
                v: Image {
                    data: &v,
                    stride: cs,
                },
            };
            c.jpeg_blocks(pixels, (w, h), &q, &mut got).unwrap();
            assert!(got == want, "I420 {w}x{h}");
            let uvs = 2 * cw + pad;
            let mut uv = vec![0u8; uvs * ch];
            for r in 0..ch {
                for x in 0..cw {
                    uv[r * uvs + 2 * x] = u[r * cs + x];
                    uv[r * uvs + 2 * x + 1] = v[r * cs + x];
                }
            }
            let pixels = JpegPixels::Nv12 {
                y: image,
                uv: Image {
                    data: &uv,
                    stride: uvs,
                },
            };
            c.jpeg_blocks(pixels, (w, h), &q, &mut got).unwrap();
            assert!(got == want, "NV12 {w}x{h}");
        }
    }
}
