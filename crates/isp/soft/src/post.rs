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

use crate::output::OutputBuffers;
use crate::params::{Denoise, Sharpen};
use crate::simd::post::{self as kernels, Sharpening};
use crate::simd::{YuvCoeffs, interleave_rgb_row, rgb_to_uv_row, rgb_to_y_row};

#[cfg(not(feature = "std"))]
use styx_math::Float as _;

/// Quarter codes: thresholds are kept with two fractional bits.
const Q4: f32 = 4.0;
/// The stages' tables, from the parameters.
#[derive(Debug)]
pub(crate) struct PostPrep {
    /// Luma sigma threshold (quarter codes) per centre value, and the strength in Q8.
    luma: Option<([u16; 256], i16)>,
    /// Chroma threshold (quarter codes) and strength in Q8.
    chroma: Option<(i16, i16)>,
    /// Sharpening.
    sharpen: Option<Sharpening>,
    /// The output's colour matrix.
    pub out: YuvCoeffs,
}

fn q8(s: f32) -> i16 {
    (s.clamp(0.0, 1.0) * 256.0 + 0.5) as i16
}

impl PostPrep {
    /// The stages `d` and `s` ask for, or `None` when neither does anything.
    pub fn new(d: Option<&Denoise>, s: Option<&Sharpen>, out: YuvCoeffs) -> Option<Self> {
        let luma = d.and_then(|d| d.luma).map(|l| {
            let table = core::array::from_fn(|c| {
                let x16 = (c * 257) as f32;
                let var = (l.constant + l.slope * x16).max(0.0);
                // Sigma in 16-bit codes, then in 8-bit codes, in quarters.
                ((var.sqrt() / 257.0 * Q4 + 0.5) as u16).min(255 * 4)
            });
            (table, q8(l.strength))
        });
        let chroma = d.and_then(|d| d.chroma).map(|c| {
            (
                ((c.threshold.max(0.0) / 257.0 * Q4 + 0.5) as i16).clamp(0, 255 * 4),
                q8(c.strength),
            )
        });
        let sharpen = s.map(|s| Sharpening {
            amount: (s.amount.max(0.0) * 256.0 + 0.5) as i32,
            threshold: (s.threshold.max(0.0) + 0.5) as i32,
            limit: (s.limit.max(0.0) + 0.5) as i32,
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

/// The output stages' tables, as the output stages build them, for other implementations of the
/// stages (the GPU ISP): the luma thresholds per centre value and the strength in Q8, the
/// chroma threshold (quarter codes) and strength in Q8, and the sharpening amount in Q8,
/// threshold and limit.
#[derive(Clone, Debug, PartialEq)]
pub struct StageTables {
    pub luma: Option<([u16; 256], i16)>,
    pub chroma: Option<(i16, i16)>,
    pub sharpen: Option<[i32; 3]>,
}

/// The [`StageTables`] of `d` and `s`, or `None` when neither stage does anything.
pub fn stage_tables(d: Option<&Denoise>, s: Option<&Sharpen>) -> Option<StageTables> {
    // The output matrix is not part of the tables.
    let p = PostPrep::new(d, s, YuvCoeffs::BT601_FULL)?;
    Some(StageTables {
        luma: p.luma,
        chroma: p.chroma,
        sharpen: p.sharpen.map(|s| [s.amount, s.threshold, s.limit]),
    })
}

/// Rows `y0 .. y0 + rows` of the three planes of a `w x h` picture, for one band.
pub(crate) struct Band<'a> {
    pub y0: usize,
    pub rows: usize,
    pub planes: [&'a mut [u8]; 3],
}

/// Split the three planes of a `w x h` picture held in `buf` (`3 w h` bytes) into bands of
/// `band` rows.
pub(crate) fn bands(
    buf: &mut [u8],
    w: usize,
    h: usize,
    band: usize,
) -> impl ExactSizeIterator<Item = Band<'_>> + Send {
    let plane = w * h;
    let (y, rest) = buf[..3 * plane].split_at_mut(plane);
    let (cb, cr) = rest.split_at_mut(plane);
    let step = band.max(1) * w;
    y.chunks_mut(step)
        .zip(cb.chunks_mut(step))
        .zip(cr.chunks_mut(step))
        .enumerate()
        .map(move |(k, ((y, cb), cr))| Band {
            y0: k * band,
            rows: y.len() / w,
            planes: [y, cb, cr],
        })
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
        kernels::rgb_to_planes_row(
            &rgb[(band.y0 + i) * 3 * w..],
            &mut py[i * w..],
            &mut pcb[i * w..],
            &mut pcr[i * w..],
            w,
        );
    }
}

/// RGB of a planes pixel (JFIF inverse, rounded and clamped).
#[cfg(test)]
pub(crate) fn rgb_of(y: i32, cb: i32, cr: i32) -> [i32; 3] {
    let (cb, cr) = (cb - 128, cr - 128);
    [
        ((256 * y + 359 * cr + 128) >> 8).clamp(0, 255),
        ((256 * y - 88 * cb - 183 * cr + 128) >> 8).clamp(0, 255),
        ((256 * y + 454 * cb + 128) >> 8).clamp(0, 255),
    ]
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
            let rows = [
                &src_plane[reflect(y as isize - 1, h) * w..][..w],
                &src_plane[y * w..][..w],
                &src_plane[reflect(y as isize + 1, h) * w..][..w],
            ];
            match (k, &p.luma, &p.chroma) {
                (0, Some((table, s)), _) => kernels::sigma_row(rows, dst, table, *s),
                (_, _, Some((t, s))) => kernels::chroma_row(rows, dst, *t, *s),
                _ => unreachable!("checked above"),
            };
        }
    }
}

/// The sharpening stage on one band: luma unsharp-masked, the chroma planes copied.
pub(crate) fn sharpen_band(p: &PostPrep, src: &[u8], w: usize, h: usize, band: &mut Band<'_>) {
    let plane = w * h;
    let sharpen = p.sharpen.expect("sharpening on");
    let rows = band.rows * w;
    let off = band.y0 * w;
    let [py, pcb, pcr] = &mut band.planes;
    pcb[..rows].copy_from_slice(&src[plane + off..][..rows]);
    pcr[..rows].copy_from_slice(&src[2 * plane + off..][..rows]);
    let luma = &src[..plane];
    for (i, dst) in py[..rows].chunks_exact_mut(w).enumerate() {
        let y = band.y0 + i;
        let rows = [
            &luma[reflect(y as isize - 1, h) * w..][..w],
            &luma[y * w..][..w],
            &luma[reflect(y as isize + 1, h) * w..][..w],
        ];
        kernels::sharpen_row(rows, dst, sharpen);
    }
}

/// Output pixels: the cropped picture's origin in the planes.
pub(crate) struct Crop {
    pub x0: usize,
    pub y0: usize,
}

/// Pixels of output converted at a time, on the stack: even, so a chunk ends on a chroma pair.
const CHUNK: usize = 256;

/// One chunk of planar RGB.
type Chunk = [[u8; CHUNK]; 3];

/// `n` pixels of RGB from picture row `row` (cropped at `crop`, of `w x h` planes), column `x`.
fn picture_rgb(
    planes: &[u8],
    (w, h): (usize, usize),
    crop: &Crop,
    (row, x, n): (usize, usize, usize),
    out: &mut Chunk,
) {
    let plane = w * h;
    let r = (crop.y0 + row) * w + crop.x0 + x;
    let [red, green, blue] = out;
    kernels::planes_to_rgb_row(
        [
            &planes[r..][..n],
            &planes[plane + r..][..n],
            &planes[2 * plane + r..][..n],
        ],
        [red, green, blue],
        n,
    );
}

fn view(c: &Chunk) -> [&[u8]; 3] {
    [&c[0], &c[1], &c[2]]
}

/// The columns of an `ow`-wide output, a chunk at a time, as `(x, n)`.
fn chunks(ow: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..ow).step_by(CHUNK).map(move |x| (x, CHUNK.min(ow - x)))
}

/// Output rows `o0 .. o0 + rows` of an `ow`-wide output, one band of it, from the processed
/// planes of `w x h` cropped at `crop`. Even `o0` and `rows` for 4:2:0. 4:2:0 chroma is the
/// rounded mean of each 2x2 block.
pub(crate) fn write_output(
    coeffs: &YuvCoeffs,
    planes: &[u8],
    dims: (usize, usize),
    crop: &Crop,
    (ow, o0, rows): (usize, usize, usize),
    out: OutputBuffers<'_>,
) {
    let mut top: Chunk = [[0; CHUNK]; 3];
    let mut bottom: Chunk = [[0; CHUNK]; 3];
    match out {
        OutputBuffers::Rgb24 { data, stride } => {
            for i in 0..rows {
                for (x, n) in chunks(ow) {
                    picture_rgb(planes, dims, crop, (o0 + i, x, n), &mut top);
                    interleave_rgb_row(view(&top), &mut data[i * stride + 3 * x..], n);
                }
            }
        }
        OutputBuffers::Luma { data, stride } => {
            for i in 0..rows {
                for (x, n) in chunks(ow) {
                    picture_rgb(planes, dims, crop, (o0 + i, x, n), &mut top);
                    rgb_to_y_row(view(&top), &mut data[i * stride + x..], n, coeffs);
                }
            }
        }
        OutputBuffers::Nv12 {
            y,
            y_stride,
            uv,
            uv_stride,
        } => {
            for i in (0..rows).step_by(2) {
                for (x, n) in chunks(ow) {
                    picture_rgb(planes, dims, crop, (o0 + i, x, n), &mut top);
                    picture_rgb(planes, dims, crop, (o0 + i + 1, x, n), &mut bottom);
                    rgb_to_y_row(view(&top), &mut y[i * y_stride + x..], n, coeffs);
                    rgb_to_y_row(view(&bottom), &mut y[(i + 1) * y_stride + x..], n, coeffs);
                    rgb_to_uv_row(
                        view(&top),
                        view(&bottom),
                        &mut uv[i / 2 * uv_stride + x..],
                        &mut [],
                        n / 2,
                        coeffs,
                        true,
                    );
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
            for i in (0..rows).step_by(2) {
                for (x, n) in chunks(ow) {
                    picture_rgb(planes, dims, crop, (o0 + i, x, n), &mut top);
                    picture_rgb(planes, dims, crop, (o0 + i + 1, x, n), &mut bottom);
                    rgb_to_y_row(view(&top), &mut y[i * y_stride + x..], n, coeffs);
                    rgb_to_y_row(view(&bottom), &mut y[(i + 1) * y_stride + x..], n, coeffs);
                    rgb_to_uv_row(
                        view(&top),
                        view(&bottom),
                        &mut u[i / 2 * u_stride + x / 2..],
                        &mut v[i / 2 * v_stride + x / 2..],
                        n / 2,
                        coeffs,
                        false,
                    );
                }
            }
        }
    }
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
