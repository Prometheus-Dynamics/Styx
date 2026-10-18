//! The output picture's stages: luma and colour denoise, then luma sharpening, after the tone
//! curve (see [`crate::Denoise`] and [`crate::Sharpen`]).
//!
//! The stages work on the 8-bit picture the pipeline makes, as planes of JFIF (BT.601 full
//! range) YCbCr: `Y`, `Cb`, `Cr` each `width x height`, one after another in one buffer. Each
//! stage is a row kernel over a band of rows, reading the whole source planes (the neighbours
//! of a band's edge rows are in the source, so bands need no halo) and writing the band's own
//! rows, so the bands run on the worker threads. The output conversion then writes the
//! requested format from the planes, with the colour matrix of the output's
//! [`crate::YuvMatrix`] applied to the processed RGB.
//!
//! The denoise filter is a sigma filter over the 3x3 neighbourhood: a neighbour is averaged in
//! when it differs from the centre by at most the threshold (the threshold of luma is a
//! function of the centre's value), then the result is blended in by the strength. Flat areas
//! stay flat exactly, and an edge larger than the threshold is not blurred. Sharpening is an
//! unsharp mask with the blur `[1 2 1] x [1 2 1] / 16`, the detail cored by its threshold and
//! the correction limited.

use alloc::vec::Vec;

use crate::output::OutputBuffers;
use crate::params::{Denoise, Sharpen};
use crate::simd::YuvCoeffs;
use crate::simd::scalar::chroma;

#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

/// Quarter codes: thresholds are kept with two fractional bits.
const Q4: f32 = 4.0;
/// `round(65536 / n)` for the counts of the 3x3 sigma filter (1 to 9): the mean of `n` values
/// with sum `s` is `(s RCP[n] + 2^15) >> 16`, exact for a flat field (`n RCP[n]` is within `n`
/// of `2^16`).
const RCP: [i32; 10] = [
    0, 65536, 32768, 21845, 16384, 13107, 10923, 9362, 8192, 7282,
];

/// The stages' tables, from the parameters.
#[derive(Debug)]
pub(crate) struct PostPrep {
    /// Luma sigma threshold (quarter codes) per centre value, and the strength in Q8.
    luma: Option<(Vec<i32>, i32)>,
    /// Chroma threshold (quarter codes) and strength in Q8.
    chroma: Option<(i32, i32)>,
    /// Sharpening: amount in Q8, threshold and limit in codes.
    sharpen: Option<(i32, i32, i32)>,
    /// The output's colour matrix.
    pub out: YuvCoeffs,
}

fn q8(s: f32) -> i32 {
    (s.clamp(0.0, 1.0) * 256.0 + 0.5) as i32
}

impl PostPrep {
    /// The stages `d` and `s` ask for, or `None` when neither does anything.
    pub fn new(d: Option<&Denoise>, s: Option<&Sharpen>, out: YuvCoeffs) -> Option<Self> {
        let luma = d.and_then(|d| d.luma).map(|l| {
            let table = (0..256)
                .map(|c| {
                    let x16 = (c * 257) as f32;
                    let var = (l.constant + l.slope * x16).max(0.0);
                    // Sigma in 16-bit codes, then in 8-bit codes, in quarters.
                    ((var.sqrt() / 257.0 * Q4 + 0.5) as i32).min(255 * 4)
                })
                .collect();
            (table, q8(l.strength))
        });
        let chroma = d.and_then(|d| d.chroma).map(|c| {
            (
                ((c.threshold.max(0.0) / 257.0 * Q4 + 0.5) as i32).min(255 * 4),
                q8(c.strength),
            )
        });
        let sharpen = s.map(|s| {
            (
                (s.amount.max(0.0) * 256.0 + 0.5) as i32,
                (s.threshold.max(0.0) + 0.5) as i32,
                (s.limit.max(0.0) + 0.5) as i32,
            )
        });
        if luma.is_none() && chroma.is_none() && sharpen.is_none() {
            return None;
        }
        Some(Self {
            luma,
            chroma,
            sharpen,
            out,
        })
    }

    /// Whether the denoise stage runs (either part).
    pub fn denoises(&self) -> bool {
        self.luma.is_some() || self.chroma.is_some()
    }

    /// Whether the sharpening stage runs.
    pub fn sharpens(&self) -> bool {
        self.sharpen.is_some()
    }
}

/// Rows `y0 .. y0 + rows` of the three planes of a `w x h` picture, for one band.
pub(crate) struct Band<'a> {
    pub y0: usize,
    pub rows: usize,
    pub planes: [&'a mut [u8]; 3],
}

/// Split the three planes of a `w x h` picture held in `buf` (`3 w h` bytes) into bands of
/// `band` rows.
pub(crate) fn bands(buf: &mut [u8], w: usize, h: usize, band: usize) -> Vec<Band<'_>> {
    let plane = w * h;
    let (y, rest) = buf[..3 * plane].split_at_mut(plane);
    let (cb, cr) = rest.split_at_mut(plane);
    let step = band.max(1) * w;
    y.chunks_mut(step)
        .zip(cb.chunks_mut(step))
        .zip(cr.chunks_mut(step))
        .enumerate()
        .map(|(k, ((y, cb), cr))| Band {
            y0: k * band,
            rows: y.len() / w,
            planes: [y, cb, cr],
        })
        .collect()
}

/// Mirror index `i` into `0..n` (as the demosaic's frame edges).
#[inline(always)]
fn reflect(i: isize, n: usize) -> usize {
    let n = n as isize;
    let r = if i < 0 {
        -i
    } else if i >= n {
        2 * n - 2 - i
    } else {
        i
    };
    r.clamp(0, n - 1) as usize
}

/// RGB (interleaved, the whole `w`-wide picture) to the planes of `band`.
pub(crate) fn to_planes(rgb: &[u8], w: usize, band: &mut Band<'_>) {
    let [py, pcb, pcr] = &mut band.planes;
    for i in 0..band.rows {
        let src = &rgb[(band.y0 + i) * 3 * w..][..3 * w];
        let y = &mut py[i * w..][..w];
        let cb = &mut pcb[i * w..][..w];
        let cr = &mut pcr[i * w..][..w];
        for (((px, y), cb), cr) in src
            .as_chunks::<3>()
            .0
            .iter()
            .zip(y.iter_mut())
            .zip(cb.iter_mut())
            .zip(cr.iter_mut())
        {
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *y = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(0, 255) as u8;
            *cb = (((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            *cr = (((128 * r - 107 * g - 21 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

/// RGB of a planes pixel (JFIF inverse, rounded and clamped).
#[inline(always)]
pub(crate) fn rgb_of(y: i32, cb: i32, cr: i32) -> [i32; 3] {
    let (cb, cr) = (cb - 128, cr - 128);
    [
        ((256 * y + 359 * cr + 128) >> 8).clamp(0, 255),
        ((256 * y - 88 * cb - 183 * cr + 128) >> 8).clamp(0, 255),
        ((256 * y + 454 * cb + 128) >> 8).clamp(0, 255),
    ]
}

/// One pixel of the sigma filter: centre `c`, its eight neighbours `n` (up-left, up, up-right,
/// left, right, down-left, down, down-right), threshold `t` (quarter codes) and strength `s`
/// (Q8).
#[inline(always)]
fn sigma_px(c: i32, t: i32, s: i32, n: [i32; 8]) -> u8 {
    let (mut sum, mut cnt) = (c, 1i32);
    for v in n {
        let ok = ((v - c).abs() * 4 <= t) as i32;
        sum += ok * v;
        cnt += ok;
    }
    let avg = (sum * RCP[cnt as usize] + (1 << 15)) >> 16;
    (c + (((avg - c) * s + 128) >> 8)).clamp(0, 255) as u8
}

/// One sigma-filter row: `dst` (`w` samples) from the source rows `up`, `cur`, `dn`; `table`
/// holds the threshold (quarter codes) of each centre value.
#[inline(always)]
fn sigma_row(up: &[u8], cur: &[u8], dn: &[u8], dst: &mut [u8], table: &[i32], s: i32) {
    let w = dst.len();
    let edge = |x: usize| {
        let (xl, xr) = (reflect(x as isize - 1, w), reflect(x as isize + 1, w));
        let c = cur[x] as i32;
        let n = [
            up[xl], up[x], up[xr], cur[xl], cur[xr], dn[xl], dn[x], dn[xr],
        ]
        .map(|v| v as i32);
        sigma_px(c, table[c as usize], s, n)
    };
    if w < 3 {
        for (x, d) in dst.iter_mut().enumerate() {
            *d = edge(x);
        }
        return;
    }
    dst[0] = edge(0);
    dst[w - 1] = edge(w - 1);
    // Interior: rows cut to the row length, so the indices need no bounds checks.
    let (u, c, d) = (&up[..w], &cur[..w], &dn[..w]);
    let out = &mut dst[..w];
    for x in 1..w - 1 {
        let cc = c[x] as i32;
        let n = [
            u[x - 1],
            u[x],
            u[x + 1],
            c[x - 1],
            c[x + 1],
            d[x - 1],
            d[x],
            d[x + 1],
        ]
        .map(|v| v as i32);
        out[x] = sigma_px(cc, table[(cc & 255) as usize], s, n);
    }
}

/// One chroma row: the binomial low-pass `[1 2 1] x [1 2 1] / 16` of the three rows `up`, `cur`,
/// `dn` at each sample, taken only where it differs from the sample by at most `t` (quarter
/// codes): a colour edge larger than the noise stays. Blended in by strength `s` (Q8).
#[inline(always)]
fn chroma_row(up: &[u8], cur: &[u8], dn: &[u8], dst: &mut [u8], t: i32, s: i32) {
    let w = dst.len();
    // The vertical [1 2 1] sum of column `x`.
    let vsum = |x: usize| up[x] as i32 + 2 * cur[x] as i32 + dn[x] as i32;
    let blend = |c: i32, lo: i32, mid: i32, hi: i32| {
        let l = (lo + 2 * mid + hi + 8) >> 4;
        let ok = ((l - c).abs() * 4 <= t) as i32;
        (c + ok * (((l - c) * s + 128) >> 8)).clamp(0, 255) as u8
    };
    if w < 3 {
        for (x, d) in dst.iter_mut().enumerate() {
            let (xl, xr) = (reflect(x as isize - 1, w), reflect(x as isize + 1, w));
            *d = blend(cur[x] as i32, vsum(xl), vsum(x), vsum(xr));
        }
        return;
    }
    dst[0] = blend(cur[0] as i32, vsum(1), vsum(0), vsum(1));
    dst[w - 1] = blend(cur[w - 1] as i32, vsum(w - 2), vsum(w - 1), vsum(w - 2));
    let (u, c, d) = (&up[..w], &cur[..w], &dn[..w]);
    for x in 1..w - 1 {
        let vs = [
            u[x - 1] as i32 + 2 * c[x - 1] as i32 + d[x - 1] as i32,
            u[x] as i32 + 2 * c[x] as i32 + d[x] as i32,
            u[x + 1] as i32 + 2 * c[x + 1] as i32 + d[x + 1] as i32,
        ];
        dst[x] = blend(c[x] as i32, vs[0], vs[1], vs[2]);
    }
}

/// The denoise stage on one band: `src` the whole planes of the previous stage. A stage that
/// is off copies its plane.
pub(crate) fn denoise_band(p: &PostPrep, src: &[u8], w: usize, h: usize, band: &mut Band<'_>) {
    let plane = w * h;
    let rows = band.rows * w;
    let off = band.y0 * w;
    for (k, dst_plane) in band.planes.iter_mut().enumerate() {
        let src_plane = &src[k * plane..][..plane];
        let dst_all = &mut dst_plane[..rows];
        let on = matches!(
            (k, &p.luma, &p.chroma),
            (0, Some(_), _) | (1 | 2, _, Some(_))
        );
        if !on {
            dst_all.copy_from_slice(&src_plane[off..][..rows]);
            continue;
        }
        for (i, dst) in dst_all.chunks_exact_mut(w).enumerate() {
            let y = band.y0 + i;
            let up = &src_plane[reflect(y as isize - 1, h) * w..][..w];
            let cur = &src_plane[y * w..][..w];
            let dn = &src_plane[reflect(y as isize + 1, h) * w..][..w];
            match (k, &p.luma, &p.chroma) {
                (0, Some((table, s)), _) => sigma_row(up, cur, dn, dst, table, *s),
                (_, _, Some((t, s))) => chroma_row(up, cur, dn, dst, *t, *s),
                _ => unreachable!("checked above"),
            }
        }
    }
}

/// The sharpening stage on one band: luma unsharp-masked, the chroma planes copied.
pub(crate) fn sharpen_band(p: &PostPrep, src: &[u8], w: usize, h: usize, band: &mut Band<'_>) {
    let plane = w * h;
    let (amount, thr, limit) = p.sharpen.expect("sharpening on");
    let rows = band.rows * w;
    let off = band.y0 * w;
    let [py, pcb, pcr] = &mut band.planes;
    pcb[..rows].copy_from_slice(&src[plane + off..][..rows]);
    pcr[..rows].copy_from_slice(&src[2 * plane + off..][..rows]);
    let luma = &src[..plane];
    for (i, dst) in py[..rows].chunks_exact_mut(w).enumerate() {
        let y = band.y0 + i;
        let up = &luma[reflect(y as isize - 1, h) * w..][..w];
        let cur = &luma[y * w..][..w];
        let dn = &luma[reflect(y as isize + 1, h) * w..][..w];
        // The sharpened sample at column `x`, its neighbours at `xl` and `xr`.
        let correct = |x: usize, xl: usize, xr: usize| {
            let c = cur[x] as i32;
            let blur = (up[xl] as i32
                + 2 * up[x] as i32
                + up[xr] as i32
                + 2 * cur[xl] as i32
                + 4 * c
                + 2 * cur[xr] as i32
                + dn[xl] as i32
                + 2 * dn[x] as i32
                + dn[xr] as i32
                + 8)
                >> 4;
            let detail = c - blur;
            let cored = detail.signum() * (detail.abs() - thr).max(0);
            let corr = ((amount * cored + 128) >> 8).clamp(-limit, limit);
            (c + corr).clamp(0, 255) as u8
        };
        if w < 3 {
            for (x, d) in dst.iter_mut().enumerate() {
                *d = correct(x, reflect(x as isize - 1, w), reflect(x as isize + 1, w));
            }
            continue;
        }
        dst[0] = correct(0, 1, 1);
        dst[w - 1] = correct(w - 1, w - 2, w - 2);
        for (x, d) in dst[1..w - 1].iter_mut().enumerate() {
            *d = correct(x + 1, x, x + 2);
        }
    }
}

/// Output pixels: the cropped picture's origin in the planes.
pub(crate) struct Crop {
    pub x0: usize,
    pub y0: usize,
}

/// RGB of picture row `row` (cropped at `crop`, of `w x h` planes), `dst.len() / 3` pixels.
fn picture_rgb(planes: &[u8], (w, h): (usize, usize), crop: &Crop, row: usize, dst: &mut [u8]) {
    let plane = w * h;
    let n = dst.len() / 3;
    let r = (crop.y0 + row) * w + crop.x0;
    let (ys, cbs, crs) = (
        &planes[r..][..n],
        &planes[plane + r..][..n],
        &planes[2 * plane + r..][..n],
    );
    for (((px, &y), &cb), &cr) in dst
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(ys)
        .zip(cbs)
        .zip(crs)
    {
        let p = rgb_of(y as i32, cb as i32, cr as i32);
        px[0] = p[0] as u8;
        px[1] = p[1] as u8;
        px[2] = p[2] as u8;
    }
}

/// Output rows `o0 .. o0 + rows` of an `ow`-wide output, one band of it, from the processed
/// planes of `w x h` cropped at `crop`. Even `o0` and `rows` for 4:2:0.
pub(crate) fn write_output(
    coeffs: &YuvCoeffs,
    planes: &[u8],
    (w, h): (usize, usize),
    crop: &Crop,
    (ow, o0, rows): (usize, usize, usize),
    out: OutputBuffers<'_>,
) {
    let luma = |px: &[u8]| {
        let s = coeffs.y[0] as i32 * px[0] as i32
            + coeffs.y[1] as i32 * px[1] as i32
            + coeffs.y[2] as i32 * px[2] as i32;
        (((s + 128) >> 8) + coeffs.y_offset as i32).clamp(0, 255) as u8
    };
    match out {
        OutputBuffers::Rgb24 { data, stride } => {
            for i in 0..rows {
                picture_rgb(
                    planes,
                    (w, h),
                    crop,
                    o0 + i,
                    &mut data[i * stride..][..3 * ow],
                );
            }
        }
        OutputBuffers::Luma { data, stride } => {
            let mut tmp = alloc::vec![0u8; 3 * ow];
            for i in 0..rows {
                picture_rgb(planes, (w, h), crop, o0 + i, &mut tmp);
                for (d, px) in data[i * stride..][..ow]
                    .iter_mut()
                    .zip(tmp.as_chunks::<3>().0.iter())
                {
                    *d = luma(px);
                }
            }
        }
        OutputBuffers::Nv12 {
            y,
            y_stride,
            uv,
            uv_stride,
        } => {
            let mut tmp = alloc::vec![0u8; 6 * ow];
            for i in (0..rows).step_by(2) {
                luma_pair(planes, (w, h), crop, o0 + i, &mut tmp, |r, px| {
                    for (d, p) in y[(i + r) * y_stride..][..ow]
                        .iter_mut()
                        .zip(px.as_chunks::<3>().0.iter())
                    {
                        *d = luma(p);
                    }
                });
                let row = &mut uv[i / 2 * uv_stride..][..ow];
                for (j, pair) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    (pair[0], pair[1]) = chroma_quad(coeffs, &tmp, ow, 2 * j);
                }
            }
        }
        OutputBuffers::I420 {
            y,
            y_stride,
            u,
            u_stride,
            v,
            v_stride,
        } => {
            let mut tmp = alloc::vec![0u8; 6 * ow];
            for i in (0..rows).step_by(2) {
                luma_pair(planes, (w, h), crop, o0 + i, &mut tmp, |r, px| {
                    for (d, p) in y[(i + r) * y_stride..][..ow]
                        .iter_mut()
                        .zip(px.as_chunks::<3>().0.iter())
                    {
                        *d = luma(p);
                    }
                });
                for j in 0..ow / 2 {
                    let (cu, cv) = chroma_quad(coeffs, &tmp, ow, 2 * j);
                    u[i / 2 * u_stride + j] = cu;
                    v[i / 2 * v_stride + j] = cv;
                }
            }
        }
    }
}

/// The two picture rows `row` and `row + 1` as RGB in `tmp` (`ow` pixels each), handed to
/// `each` with their index in the pair.
fn luma_pair(
    planes: &[u8],
    dims: (usize, usize),
    crop: &Crop,
    row: usize,
    tmp: &mut [u8],
    mut each: impl FnMut(usize, &[u8]),
) {
    let half = tmp.len() / 2;
    let (top, bottom) = tmp.split_at_mut(half);
    picture_rgb(planes, dims, crop, row, top);
    picture_rgb(planes, dims, crop, row + 1, bottom);
    each(0, top);
    each(1, bottom);
}

/// The chroma of the 2x2 block at column `x` of the two RGB rows in `tmp` (`ow` pixels each):
/// the rounded mean of its four pixels, as the existing 4:2:0 output.
fn chroma_quad(coeffs: &YuvCoeffs, tmp: &[u8], ow: usize, x: usize) -> (u8, u8) {
    let mut sum = [0i32; 3];
    for row in 0..2 {
        for col in 0..2 {
            let px = &tmp[(row * ow + x + col) * 3..][..3];
            for k in 0..3 {
                sum[k] += px[k] as i32;
            }
        }
    }
    let mean = sum.map(|s| (s + 2) >> 2);
    (
        chroma(&coeffs.u, mean[0], mean[1], mean[2]),
        chroma(&coeffs.v, mean[0], mean[1], mean[2]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{ChromaDenoise, LumaDenoise};
    use crate::simd::YuvCoeffs;
    use alloc::vec;

    fn luma_only(constant: f32, slope: f32, strength: f32) -> Denoise {
        Denoise {
            luma: Some(LumaDenoise {
                constant,
                slope,
                strength,
            }),
            chroma: None,
        }
    }

    /// The three planes of a `w x h` picture from a function of (x, y).
    fn picture(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut buf = vec![0u8; 3 * w * h];
        for y in 0..h {
            for x in 0..w {
                for k in 0..3 {
                    buf[k * w * h + y * w + x] = f(x, y)[k];
                }
            }
        }
        buf
    }

    /// One stage over the whole picture, as one band.
    fn run(stage: impl Fn(&mut Band<'_>), w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; 3 * w * h];
        for mut band in bands(&mut out, w, h, h) {
            stage(&mut band);
        }
        out
    }

    fn plane(buf: &[u8], w: usize, h: usize, k: usize) -> &[u8] {
        &buf[k * w * h..][..w * h]
    }

    #[test]
    fn flat_field_stays_flat_under_every_stage() {
        let (w, h) = (24, 16);
        let src = picture(w, h, |_, _| [90, 140, 200]);
        let p = PostPrep::new(
            Some(&Denoise {
                luma: Some(LumaDenoise {
                    constant: 400.0,
                    slope: 20.0,
                    strength: 1.0,
                }),
                chroma: Some(ChromaDenoise {
                    threshold: 3000.0,
                    strength: 1.0,
                }),
            }),
            Some(&Sharpen {
                amount: 1.0,
                threshold: 0.0,
                limit: 30.0,
            }),
            YuvCoeffs::BT601_FULL,
        )
        .unwrap();
        let d = run(|b| denoise_band(&p, &src, w, h, b), w, h);
        assert_eq!(d, src, "denoise of a flat field");
        let s = run(|b| sharpen_band(&p, &d, w, h, b), w, h);
        assert_eq!(s, src, "sharpening of a flat field");
    }

    #[test]
    fn strength_zero_is_the_identity() {
        // Strength 0 (both parts) leaves every sample alone, bit for bit.
        let (w, h) = (17, 11);
        let src = picture(w, h, |x, y| {
            [(x * 13 + y * 7) as u8, (x * 3) as u8, (y * 20) as u8]
        });
        let p = PostPrep::new(
            Some(&Denoise {
                luma: Some(LumaDenoise {
                    constant: 1e6,
                    slope: 0.0,
                    strength: 0.0,
                }),
                chroma: Some(ChromaDenoise {
                    threshold: 1e6,
                    strength: 0.0,
                }),
            }),
            None,
            YuvCoeffs::BT601_FULL,
        )
        .unwrap();
        assert_eq!(run(|b| denoise_band(&p, &src, w, h, b), w, h), src);
    }

    #[test]
    fn denoise_smooths_noise_and_keeps_an_edge() {
        let (w, h) = (32, 8);
        // Left half 60 with +-3 noise, right half 180.
        let noisy = |x: usize, y: usize| {
            let n = [3i32, -2, 0, 2, -3, 1, -1, 0][(x * 5 + y) % 8];
            let v = if x < w / 2 { 60 + n } else { 180 };
            [v as u8, 128, 128]
        };
        let src = picture(w, h, noisy);
        let p = PostPrep::new(
            Some(&luma_only(0.0, 69.0, 1.0)),
            None,
            YuvCoeffs::BT601_FULL,
        )
        .unwrap();
        let out = run(|b| denoise_band(&p, &src, w, h, b), w, h);
        let y = plane(&out, w, h, 0);
        let spread = |pix: &[u8]| {
            let (mut lo, mut hi) = (255u8, 0u8);
            for r in 0..h {
                for x in 2..w / 2 - 2 {
                    lo = lo.min(pix[r * w + x]);
                    hi = hi.max(pix[r * w + x]);
                }
            }
            hi - lo
        };
        assert!(
            spread(y) < spread(plane(&src, w, h, 0)),
            "noise not reduced"
        );
        assert!(
            (y[3 * w + w / 2] as i32 - 180).abs() <= 1,
            "right side blurred"
        );
        assert!(
            (y[3 * w + w / 2 - 1] as i32 - 60).abs() <= 3,
            "left side pulled"
        );
    }

    #[test]
    fn sharpening_steepens_an_edge_within_its_limit() {
        let (w, h) = (16, 6);
        let src = picture(w, h, |x, _| {
            let v = if x < w / 2 { 100 } else { 160 };
            [v, 128, 128]
        });
        let s = Sharpen {
            amount: 0.5,
            threshold: 0.0,
            limit: 6.0,
        };
        let p = PostPrep::new(None, Some(&s), YuvCoeffs::BT601_FULL).unwrap();
        let out = run(|b| sharpen_band(&p, &src, w, h, b), w, h);
        let y = plane(&out, w, h, 0);
        let row = &y[2 * w..3 * w];
        // Undershoot on the dark side, overshoot on the bright side, within the limit.
        assert!(
            row[w / 2 - 1] < 100 && row[w / 2 - 1] >= 94,
            "dark side {row:?}"
        );
        assert!(row[w / 2] > 160 && row[w / 2] <= 166, "bright side {row:?}");
        // Flat parts are untouched.
        assert_eq!(row[1], 100);
        assert_eq!(row[w - 2], 160);
        // Chroma copied.
        assert_eq!(plane(&out, w, h, 1), plane(&src, w, h, 1));
    }

    #[test]
    fn sharpening_threshold_leaves_small_detail_alone() {
        let (w, h) = (16, 6);
        let src = picture(w, h, |x, _| [if x % 2 == 0 { 100 } else { 103 }, 128, 128]);
        let s = Sharpen {
            amount: 1.0,
            threshold: 8.0,
            limit: 30.0,
        };
        let p = PostPrep::new(None, Some(&s), YuvCoeffs::BT601_FULL).unwrap();
        let out = run(|b| sharpen_band(&p, &src, w, h, b), w, h);
        assert_eq!(plane(&out, w, h, 0), plane(&src, w, h, 0));
    }

    #[test]
    fn planes_round_trip_within_two_codes() {
        let (w, h) = (8, 4);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| ((i * 37) % 256) as u8).collect();
        let mut planes = vec![0u8; 3 * w * h];
        for mut band in bands(&mut planes, w, h, h) {
            to_planes(&rgb, w, &mut band);
        }
        for i in 0..w * h {
            let p = rgb_of(
                planes[i] as i32,
                planes[w * h + i] as i32,
                planes[2 * w * h + i] as i32,
            );
            for k in 0..3 {
                assert!(
                    (p[k] - rgb[3 * i + k] as i32).abs() <= 2,
                    "pixel {i} channel {k}"
                );
            }
        }
    }

    #[test]
    fn none_when_nothing_is_on() {
        assert!(PostPrep::new(None, None, YuvCoeffs::BT601_FULL).is_none());
        assert!(PostPrep::new(Some(&Denoise::default()), None, YuvCoeffs::BT601_FULL).is_none());
    }
}
